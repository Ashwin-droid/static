use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use dbus::ffidisp::{BusType as DbusBusType, Connection as DbusConnection};
use gstreamer::ClockTime;
use gstreamer::glib;
use mpris::{PlaybackStatus as PeerPlaybackStatus, Player as PeerMprisPlayer};
use mpris_server::{
    LoopStatus as MprisLoopStatus, Metadata, PlaybackStatus as MprisPlaybackStatus,
    Player as MprisPlayer, Time as MprisTime, TrackId,
};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::flag;
use zbus::blocking::{Connection, Proxy};
use zbus::interface;

use crate::ipc::{
    ActiveTrackSnapshot, CoordinationTargetSnapshot, CoordinationTargetsSnapshot, DAEMON_BUS_NAME,
    DAEMON_OBJECT_PATH, DaemonRequest, DaemonResponse, PersistedPlaybackState, PlaybackSnapshot,
    PlaylistContext, RepeatMode, SleepStatusSnapshot, clear_persisted_playback_state,
    current_unix_millis, store_persisted_playback_state, store_persisted_playlist_state,
    youtube_music_watch_url,
};
use crate::models::{RadioChip, RadioVirtualPlaylist, Track};
use crate::network::{
    SongRating, SponsorBlockClient, SponsorSegment, TrackingInfo, default_skip_categories,
    download_media_bytes, fetch_thumbnail_bytes, filter_segments_for_categories,
    sanitize_categories,
};
use crate::network_state::NetworkState;
use crate::offline::{
    AppSettings, CoordinationMode, MusicCache, PIPEWIRE_DUCKING_VOLUME_CLI_MAX,
    coordination_filter_allows, normalize_coordination_identifier,
};
use crate::pipewire_monitor::PipeWireMonitor;
use crate::playback_engine::PlaybackEngine;

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        crate::app_log!($($arg)*);
    }};
}

const UI_APP_OBJECT_PATH: &str = "/com/static/music";
const MIN_LISTEN_SECS: u64 = 10;
const PAUSED_SHUTDOWN_AFTER: Duration = Duration::from_secs(20 * 60);
const PLAYBACK_START_TIMEOUT: Duration = Duration::from_secs(12);
const PLAYBACK_START_RETRY_LIMIT: u8 = 2;
const CACHE_MEDIA_DOWNLOAD_ATTEMPTS: usize = 3;
const PLAYBACK_START_MIN_CONFIRMED_POSITION_MILLIS: u64 = 250;
const PLAYBACK_STARTED_POSITION_THRESHOLD_MILLIS: u64 = 1_000;
const PLAYBACK_START_SEEK_CONFIRM_LEEWAY_MILLIS: u64 = 5_000;
const PLAYBACK_STATE_PERSIST_INTERVAL: Duration = Duration::from_secs(1);
const PREVIOUS_TRACK_THRESHOLD_MILLIS: u64 = 3_000;
const MPRIS_POSITION_SYNC_INTERVAL: Duration = Duration::from_millis(250);
const EXTERNAL_MPRIS_POLL_INTERVAL: Duration = Duration::from_millis(250);
const EXTERNAL_MPRIS_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(30);
const MPRIS_PENDING_POSITION_SYNC_INTERVAL: Duration = Duration::from_secs(1);
const PLAYBACK_VOLUME_RAMP_DURATION: Duration = Duration::from_secs(2);
const PIPEWIRE_STREAM_POLL_INTERVAL: Duration = Duration::from_millis(500);
const PIPEWIRE_VOLUME_RAMP_DURATION: Duration = Duration::from_millis(250);
const UI_CLIENT_LEASE_TTL: Duration = Duration::from_secs(20);
const DBUS_SERVICE: &str = "org.freedesktop.DBus";
const DBUS_OBJECT_PATH: &str = "/org/freedesktop/DBus";
const DBUS_INTERFACE: &str = "org.freedesktop.DBus";
const STATIC_MPRIS_BUS_NAME: &str = "org.mpris.MediaPlayer2.static";
const PEER_MPRIS_TIMEOUT_MS: i32 = 500;

struct ActiveTrack {
    video_id: String,
    title: String,
    artist: String,
    thumb_url: Option<String>,
    playback_source: PlaybackSource,
    playback_uri: String,
    reporting_enabled: bool,
    rating: SongRating,
    tracking: Option<TrackingInfo>,
    started_at: Instant,
    last_position_millis: u64,
    sponsorblock_auto_skip_offset_millis: u64,
    sponsorblock_skipped_segment_ids: HashSet<String>,
    startup_seek_millis: Option<u64>,
    startup_retries: u8,
    awaiting_fresh_position: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaybackSource {
    OfflineCache,
    TempPrefetch,
    RemoteStream,
}

impl PlaybackSource {
    fn label(self) -> &'static str {
        match self {
            Self::OfflineCache => "offline-cache",
            Self::TempPrefetch => "temp-prefetch",
            Self::RemoteStream => "remote-stream",
        }
    }
}

struct PreparedPlayback {
    uri: String,
    source: PlaybackSource,
    video_id: String,
}

struct CacheAudioDownload {
    video_id: String,
    audio: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PipeWireSuppressionMode {
    None,
    Duck,
    Pause,
}

#[derive(Debug, Clone, Copy)]
enum VolumeRampCompletion {
    None,
    PauseAtPosition { position_millis: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaybackTransition {
    Immediate,
    Calm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaybackStartMode {
    Playing,
    Paused { position_millis: Option<u64> },
}

#[derive(Debug, Clone, Copy)]
struct VolumeRamp {
    started_at: Instant,
    start_volume: f64,
    target_volume: f64,
    duration: Duration,
    completion: VolumeRampCompletion,
}

#[derive(Debug, Clone)]
struct ProbeRestoreState {
    queue_index: usize,
    queue_video_id: Option<String>,
    position_millis: Option<u64>,
    resume_playback: bool,
    track_repeat_loops_left: Option<u32>,
}

#[derive(Clone, Copy)]
enum TrackRepeatBudgetMode {
    ResetForCurrentTrack,
    PreserveCurrent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MprisHaltAutoResumeState {
    Idle,
    Pending,
}

impl MprisHaltAutoResumeState {
    fn is_pending(self) -> bool {
        matches!(self, Self::Pending)
    }
}

#[derive(Clone, Copy)]
enum PendingMprisTransportIntent {
    Cancel(&'static str),
    ResumeWithoutApplying(&'static str),
}

impl PendingMprisTransportIntent {
    fn label(self) -> &'static str {
        match self {
            Self::Cancel(reason) | Self::ResumeWithoutApplying(reason) => reason,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingMprisTransportDecision {
    Ignore,
    Cancel,
    ResumeWithoutApplying,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MprisHaltAutoPeerTransition {
    Ignore,
    DeactivateCoordination,
    ReturnToPeerBlock,
    ClearPending,
    ActivatePending,
    Resume,
    Wait,
}

struct DaemonState {
    player: PlaybackEngine,
    cache: MusicCache,
    settings: AppSettings,
    client: Option<Rc<NetworkState>>,
    client_cookie_fingerprint: Option<u64>,
    queue: Vec<Track>,
    queue_index: usize,
    playlist_context: Option<PlaylistContext>,
    radio_virtual_playlist: Option<RadioVirtualPlaylist>,
    repeat_mode: RepeatMode,
    track_repeat_loops_left: Option<u32>,
    active_track: Option<ActiveTrack>,
    probe_context: Option<ProbeRestoreState>,
    sponsor_segments: Vec<SponsorSegment>,
    sponsor_segments_reported: HashSet<String>,
    prefetch_in_flight: HashSet<String>,
    paused_since: Option<Instant>,
    radio_append_in_flight: bool,
    last_radio_append_anchor: Option<String>,
    background_tx: mpsc::Sender<BackgroundEvent>,
    deferred_actions: VecDeque<DeferredAction>,
    offline_sync_in_flight: bool,
    offline_sync_pending: bool,
    last_duration_micros: Cell<Option<i64>>,
    last_mpris_position_millis: Cell<Option<u64>>,
    last_mpris_position_sync_at: Cell<Option<Instant>>,
    mpris: RefCell<Option<Rc<DaemonMprisBridge>>>,
    peer_mpris: ExternalMprisCoordinator,
    mpris_halt_auto_resume: MprisHaltAutoResumeState,
    pipewire_monitor: PipeWireMonitor,
    pipewire_trigger_active: bool,
    pipewire_quiet_since: Option<Instant>,
    last_pipewire_poll_at: Option<Instant>,
    ducked: bool,
    pipewire_pause_active: bool,
    pre_duck_volume: f64,
    volume_ramp: Option<VolumeRamp>,
    sleep_deadline: Option<Instant>,
    sleep_after_current: bool,
    sleep_shutdown_pending: bool,
    ui_clients: HashMap<String, Instant>,
    last_persisted_playback_json: Option<String>,
    last_persisted_playback_at: Option<Instant>,
    shutting_down: Cell<bool>,
}

struct ExternalMprisCoordinator {
    last_poll_at: Option<Instant>,
    last_error_log_at: Option<Instant>,
    known_targets: Vec<CoordinationTargetSnapshot>,
    playing_services: Vec<String>,
    last_logged_playing_services: Vec<String>,
    waiting_for_services: Vec<String>,
    no_playing_services_since: Option<Instant>,
}

#[derive(Debug, Clone)]
struct MprisPeerInfo {
    bus_name: String,
    app_key: Option<String>,
    identity: Option<String>,
    desktop_entry: Option<String>,
    playing: bool,
}

impl MprisPeerInfo {
    fn filter_key(&self) -> Option<String> {
        self.desktop_entry
            .clone()
            .or_else(|| self.app_key.clone())
            .or_else(|| self.identity.clone())
    }

    fn label(&self) -> String {
        self.identity
            .clone()
            .or_else(|| self.desktop_entry.clone())
            .or_else(|| self.app_key.clone())
            .unwrap_or_else(|| self.bus_name.clone())
    }

    fn detail(&self) -> String {
        let mut parts = vec![format!("bus={}", self.bus_name)];
        if let Some(app_key) = self.app_key.as_deref() {
            parts.push(format!("app={app_key}"));
        }
        if let Some(desktop_entry) = self.desktop_entry.as_deref() {
            parts.push(format!("desktop={desktop_entry}"));
        }
        if self.playing {
            parts.push("playing".to_string());
        } else {
            parts.push("not playing".to_string());
        }
        parts.join(" | ")
    }

    fn identifiers(&self) -> Vec<String> {
        [
            self.filter_key(),
            self.app_key.clone(),
            self.identity.clone(),
            self.desktop_entry.clone(),
        ]
        .into_iter()
        .flatten()
        .filter(|value| !value.trim().is_empty())
        .collect()
    }

    fn target_snapshot(&self) -> CoordinationTargetSnapshot {
        let key = self.filter_key().unwrap_or_default();
        CoordinationTargetSnapshot {
            filterable: !key.is_empty(),
            key,
            label: self.label(),
            detail: self.detail(),
            active: self.playing,
        }
    }
}

impl ExternalMprisCoordinator {
    fn new() -> Self {
        Self {
            last_poll_at: None,
            last_error_log_at: None,
            known_targets: Vec::new(),
            playing_services: Vec::new(),
            last_logged_playing_services: Vec::new(),
            waiting_for_services: Vec::new(),
            no_playing_services_since: None,
        }
    }

    fn poll_if_due(&mut self, settings: &AppSettings) {
        if self
            .last_poll_at
            .is_some_and(|last_poll_at| last_poll_at.elapsed() < EXTERNAL_MPRIS_POLL_INTERVAL)
        {
            return;
        }
        self.last_poll_at = Some(Instant::now());
        self.poll_now(settings);
    }

    fn poll_now(&mut self, settings: &AppSettings) {
        match self.scan_other_mpris_services() {
            Ok(peers) => {
                self.known_targets = mpris_target_snapshots(&peers);
                let services = peers
                    .iter()
                    .filter(|peer| peer.playing)
                    .filter(|peer| {
                        coordination_filter_allows(
                            settings.other_mpris_filter_mode,
                            &settings.other_mpris_filter_list,
                            &peer.identifiers(),
                        )
                    })
                    .map(|peer| peer.bus_name.clone())
                    .collect();
                self.update_playing_services(services);
            }
            Err(err) => {
                self.log_error(format!("[MPRIS] failed to inspect peer players: {err}"));
                self.known_targets.clear();
                self.update_playing_services(Vec::new());
            }
        }
    }

    fn playing_services(&self) -> &[String] {
        &self.playing_services
    }

    fn pause_other_playing_services(&mut self, settings: &AppSettings) -> Vec<String> {
        self.poll_now(settings);
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
                },
                Err(err) => self.log_error(format!(
                    "[MPRIS] failed to create peer client for {bus_name}: {err}"
                )),
            }
        }

        self.playing_services.retain(|service| {
            !paused
                .iter()
                .any(|paused_service| paused_service == service)
        });
        self.update_quiet_since();
        paused
    }

    fn scan_other_mpris_services(&mut self) -> Result<Vec<MprisPeerInfo>> {
        let services = list_other_mpris_services()?;
        let mut peers = Vec::new();

        for bus_name in services {
            match inspect_mpris_peer(&bus_name) {
                Ok(peer) => peers.push(peer),
                Err(err) => self.log_error(format!(
                    "[MPRIS] failed to inspect peer player {bus_name}: {err}"
                )),
            }
        }

        Ok(peers)
    }

    fn refresh_known_targets(&mut self) -> Vec<CoordinationTargetSnapshot> {
        match self.scan_other_mpris_services() {
            Ok(peers) => {
                self.known_targets = mpris_target_snapshots(&peers);
                self.known_targets.clone()
            }
            Err(err) => {
                self.log_error(format!("[MPRIS] failed to inspect peer players: {err}"));
                self.known_targets.clone()
            }
        }
    }

    fn set_waiting_for_services(&mut self, services: &[String]) {
        self.waiting_for_services = services.to_vec();
    }

    fn update_playing_services(&mut self, services: Vec<String>) {
        self.playing_services = services;
        self.update_quiet_since();
        self.log_playing_services_snapshot_change();
    }

    fn update_quiet_since(&mut self) {
        if self.playing_services.is_empty() {
            self.no_playing_services_since
                .get_or_insert_with(Instant::now);
        } else {
            self.no_playing_services_since = None;
        }
    }

    fn no_playing_services_for(&self) -> Option<Duration> {
        self.no_playing_services_since.map(|since| since.elapsed())
    }

    fn clear_waiting(&mut self) {
        self.waiting_for_services.clear();
    }

    fn deactivate(&mut self) {
        self.last_poll_at = None;
        self.playing_services.clear();
        self.last_logged_playing_services.clear();
        self.waiting_for_services.clear();
        self.no_playing_services_since = None;
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

fn configured_track_repeat_loops(settings: &AppSettings) -> Option<u32> {
    settings
        .auto_release_track_loops
        .then_some(settings.auto_release_track_loops_after.max(1))
}

fn snapshot_track_repeat_loops_left(
    repeat_mode: RepeatMode,
    has_current_track: bool,
    settings: &AppSettings,
) -> Option<u32> {
    if repeat_mode == RepeatMode::One && has_current_track {
        configured_track_repeat_loops(settings)
    } else {
        None
    }
}

fn next_repeat_state_after_natural_end(
    repeat_mode: RepeatMode,
    track_repeat_loops_left: Option<u32>,
) -> (RepeatMode, Option<u32>) {
    if repeat_mode != RepeatMode::One {
        return (repeat_mode, track_repeat_loops_left);
    }

    let Some(loops_left) = track_repeat_loops_left else {
        return (repeat_mode, None);
    };

    if loops_left > 0 {
        (repeat_mode, Some(loops_left - 1))
    } else {
        (RepeatMode::Off, None)
    }
}

fn reset_track_repeat_budget_for_current_track(state: &mut DaemonState) {
    state.track_repeat_loops_left =
        if state.repeat_mode == RepeatMode::One && state.active_track.is_some() {
            configured_track_repeat_loops(&state.settings)
        } else {
            None
        };
}

fn rearm_track_repeat_loops_for_snapshot(snapshot: &mut PlaybackSnapshot, settings: &AppSettings) {
    snapshot.track_repeat_loops_left = snapshot_track_repeat_loops_left(
        snapshot.repeat_mode,
        snapshot.current_track.is_some(),
        settings,
    );
}

fn advance_repeat_mode_for_natural_end(state: &mut DaemonState) -> RepeatMode {
    let previous_repeat_mode = state.repeat_mode;
    let (repeat_mode, track_repeat_loops_left) =
        next_repeat_state_after_natural_end(state.repeat_mode, state.track_repeat_loops_left);
    state.repeat_mode = repeat_mode;
    state.track_repeat_loops_left = track_repeat_loops_left;
    if previous_repeat_mode != state.repeat_mode {
        sync_mpris(state);
    }
    state.repeat_mode
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
    PrefetchFinished {
        video_id: String,
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
                if handle_pending_mpris_halt_auto_transport_intent(
                    &mut state,
                    PendingMprisTransportIntent::Cancel("MPRIS PlayPause"),
                ) {
                    return;
                }
                if state.probe_context.is_some() && !is_logically_paused(&state) {
                    if let Err(err) = restore_after_probe(&mut state, false) {
                        eprintln!("[MPRIS] failed to restore queue after probe PlayPause: {err}");
                        state.probe_context = None;
                        stop_playback(&mut state, false);
                    }
                } else {
                    toggle_pause_for_user_action(&mut state);
                }
            });
        }

        {
            let state = state.clone();
            player.connect_play(move |_| {
                let mut state = state.borrow_mut();
                if handle_pending_mpris_halt_auto_transport_intent(
                    &mut state,
                    PendingMprisTransportIntent::ResumeWithoutApplying("Play"),
                ) {
                    return;
                }
                if state.active_track.is_some() && is_logically_paused(&state) {
                    resume_for_user_action(&mut state);
                }
            });
        }

        {
            let state = state.clone();
            player.connect_pause(move |_| {
                let mut state = state.borrow_mut();
                if handle_pending_mpris_halt_auto_transport_intent(
                    &mut state,
                    PendingMprisTransportIntent::Cancel("MPRIS Pause"),
                ) {
                    return;
                }
                if state.active_track.is_some() && !state.player.is_paused() {
                    if state.probe_context.is_some() {
                        if let Err(err) = restore_after_probe(&mut state, false) {
                            eprintln!("[MPRIS] failed to restore queue after probe Pause: {err}");
                            state.probe_context = None;
                            stop_playback(&mut state, false);
                        }
                    } else {
                        state.peer_mpris.clear_waiting();
                        pause_local_playback(&mut state, PlaybackTransition::Immediate);
                        sync_mpris(&state);
                    }
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
                let mut state = state.borrow_mut();
                if handle_pending_mpris_halt_auto_transport_intent(
                    &mut state,
                    PendingMprisTransportIntent::ResumeWithoutApplying("Next"),
                ) {
                    return;
                }
                let _ = advance_queue(&mut state, true);
            });
        }

        {
            let state = state.clone();
            player.connect_previous(move |_| {
                let mut state = state.borrow_mut();
                if handle_pending_mpris_halt_auto_transport_intent(
                    &mut state,
                    PendingMprisTransportIntent::ResumeWithoutApplying("Previous"),
                ) {
                    return;
                }
                let _ = handle_previous(&mut state);
            });
        }

        {
            let state = state.clone();
            player.connect_seek(move |_, offset| {
                let mut state = state.borrow_mut();
                if handle_pending_mpris_halt_auto_transport_intent(
                    &mut state,
                    PendingMprisTransportIntent::ResumeWithoutApplying("Seek"),
                ) {
                    return;
                }
                let Some(current) = state.player.position() else {
                    return;
                };
                let current_us = (current.nseconds() / 1_000) as i64;
                let target_us = (current_us + offset.as_micros()).max(0) as u64;
                let target = ClockTime::from_useconds(target_us);
                seek_to_position(&mut state, target);
            });
        }

        {
            let state = state.clone();
            player.connect_set_position(move |_, _, position| {
                let mut state = state.borrow_mut();
                if handle_pending_mpris_halt_auto_transport_intent(
                    &mut state,
                    PendingMprisTransportIntent::ResumeWithoutApplying("SetPosition"),
                ) {
                    return;
                }
                let target = ClockTime::from_useconds(position.as_micros().max(0) as u64);
                seek_to_position(&mut state, target);
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
                reset_track_repeat_budget_for_current_track(&mut state);
                sync_mpris(&state);
            });
        }

        {
            let state = state.clone();
            player.connect_set_volume(move |_, volume| {
                let mut state = state.borrow_mut();
                set_logical_volume(&mut state, volume);
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
        } else if mpris_halt_auto_resume_sidechannel_active(state) {
            MprisPlaybackStatus::Playing
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
                previous_action_position_millis(state),
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
        let position_us = mpris_position_millis(state)
            .map(|position_millis| (position_millis * 1_000) as i64)
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
        let volume = logical_volume(state);
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
    cache.clear_temp();
    let settings = cache.load_settings().unwrap_or_default();
    let player = PlaybackEngine::new();
    let initial_volume = player.volume();
    let (command_tx, command_rx) = mpsc::channel::<CommandEnvelope>();
    let (background_tx, background_rx) = mpsc::channel::<BackgroundEvent>();
    let state = Rc::new(RefCell::new(DaemonState {
        player,
        cache,
        settings,
        client: None,
        client_cookie_fingerprint: None,
        queue: Vec::new(),
        queue_index: 0,
        playlist_context: None,
        radio_virtual_playlist: None,
        repeat_mode: RepeatMode::Off,
        track_repeat_loops_left: None,
        active_track: None,
        probe_context: None,
        sponsor_segments: Vec::new(),
        sponsor_segments_reported: HashSet::new(),
        prefetch_in_flight: HashSet::new(),
        paused_since: None,
        radio_append_in_flight: false,
        last_radio_append_anchor: None,
        background_tx,
        deferred_actions: VecDeque::new(),
        offline_sync_in_flight: false,
        offline_sync_pending: false,
        last_duration_micros: Cell::new(None),
        last_mpris_position_millis: Cell::new(None),
        last_mpris_position_sync_at: Cell::new(None),
        mpris: RefCell::new(None),
        peer_mpris: ExternalMprisCoordinator::new(),
        mpris_halt_auto_resume: MprisHaltAutoResumeState::Idle,
        pipewire_monitor: PipeWireMonitor::new(),
        pipewire_trigger_active: false,
        pipewire_quiet_since: Some(Instant::now()),
        last_pipewire_poll_at: None,
        ducked: false,
        pipewire_pause_active: false,
        pre_duck_volume: initial_volume,
        volume_ramp: None,
        sleep_deadline: None,
        sleep_after_current: false,
        sleep_shutdown_pending: false,
        ui_clients: HashMap::new(),
        last_persisted_playback_json: None,
        last_persisted_playback_at: None,
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
                    sync_mpris(&state.borrow());
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

    let termination_requested = Arc::new(AtomicBool::new(false));
    flag::register(SIGTERM, termination_requested.clone())
        .context("Failed to register SIGTERM handler")?;
    flag::register(SIGINT, termination_requested.clone())
        .context("Failed to register SIGINT handler")?;

    let main_loop = glib::MainLoop::new(None, false);
    let main_loop_handle = main_loop.clone();
    let termination_requested_for_loop = termination_requested.clone();
    glib::timeout_add_local(Duration::from_millis(40), move || {
        if termination_requested_for_loop.swap(false, Ordering::SeqCst) {
            eprintln!(
                "[STATICD] received termination signal; persisting playback state before exit"
            );
            request_daemon_shutdown(&mut state.borrow_mut(), true);
            main_loop_handle.quit();
            return glib::ControlFlow::Break;
        }

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

fn request_daemon_shutdown(state: &mut DaemonState, exiting: bool) {
    state.sleep_deadline = None;
    state.sleep_after_current = false;
    state.sleep_shutdown_pending = false;
    stop_playback(state, exiting);
    state.shutting_down.set(true);
}

fn remaining_sleep_secs(deadline: Instant, now: Instant) -> u64 {
    let remaining = deadline.saturating_duration_since(now);
    remaining
        .as_secs()
        .saturating_add(u64::from(remaining.subsec_nanos() > 0))
}

fn sleep_status_snapshot(state: &DaemonState) -> SleepStatusSnapshot {
    let now = Instant::now();
    SleepStatusSnapshot {
        remaining_secs: state
            .sleep_deadline
            .map(|deadline| remaining_sleep_secs(deadline, now)),
        after_current: state.sleep_after_current,
    }
}

fn clear_sleep_state(state: &mut DaemonState) {
    state.sleep_deadline = None;
    state.sleep_after_current = false;
    state.sleep_shutdown_pending = false;
}

fn trigger_sleep_pause(state: &mut DaemonState) {
    let should_shutdown_headless = !has_live_ui_clients(state, Instant::now());
    clear_sleep_state(state);
    state.peer_mpris.clear_waiting();
    clear_mpris_halt_auto_resume_pending(state);
    pause_local_playback(state, PlaybackTransition::Calm);
    sync_mpris(state);
    maybe_persist_playback_state(state, true);
    if should_shutdown_headless {
        state.sleep_shutdown_pending = true;
        maybe_request_headless_sleep_shutdown(state, Instant::now());
    }
}

fn maybe_trigger_sleep_deadline(state: &mut DaemonState, now: Instant) -> bool {
    if !state.sleep_deadline.is_some_and(|deadline| now >= deadline) {
        return false;
    }

    trigger_sleep_pause(state);
    true
}

fn current_share_url(state: &DaemonState) -> Option<String> {
    state
        .active_track
        .as_ref()
        .map(|track| youtube_music_watch_url(&track.video_id))
}

fn note_ui_client(state: &mut DaemonState, client_id: String) {
    let client_id = client_id.trim();
    if client_id.is_empty() {
        return;
    }
    state
        .ui_clients
        .insert(client_id.to_string(), Instant::now());
    state.sleep_shutdown_pending = false;
}

fn remove_ui_client(state: &mut DaemonState, client_id: &str) {
    state.ui_clients.remove(client_id.trim());
}

fn prune_stale_ui_clients(state: &mut DaemonState, now: Instant) {
    state
        .ui_clients
        .retain(|_, last_seen| now.saturating_duration_since(*last_seen) <= UI_CLIENT_LEASE_TTL);
}

fn has_live_ui_clients(state: &mut DaemonState, now: Instant) -> bool {
    prune_stale_ui_clients(state, now);
    !state.ui_clients.is_empty()
}

fn maybe_request_headless_sleep_shutdown(state: &mut DaemonState, now: Instant) -> bool {
    if !state.sleep_shutdown_pending {
        return false;
    }
    if has_live_ui_clients(state, now) {
        state.sleep_shutdown_pending = false;
        return false;
    }
    if is_fading_out_to_pause(state) {
        return false;
    }

    eprintln!("[SLEEP] no UI clients remain after sleep pause; shutting down daemon");
    request_daemon_shutdown(state, true);
    true
}

fn dedupe_radio_recommendations(prefix: &[Track], tracks: Vec<Track>) -> Vec<Track> {
    let mut seen_ids = prefix
        .iter()
        .map(|track| track.video_id.clone())
        .collect::<HashSet<_>>();
    let mut deduped = Vec::with_capacity(tracks.len());

    for track in tracks {
        if seen_ids.insert(track.video_id.clone()) {
            deduped.push(track);
        }
    }

    deduped
}

fn normalize_radio_chip_selection(radio: &mut RadioVirtualPlaylist) -> bool {
    let previous = radio.selected_chip_index;
    radio.selected_chip_index = radio
        .selected_chip_index
        .filter(|index| *index < radio.chips.len());
    if radio.selected_chip_index.is_none() && !radio.chips.is_empty() {
        radio.selected_chip_index = Some(0);
    }
    previous != radio.selected_chip_index
}

fn daemon_owned_radio_virtual_playlist(
    state: &mut DaemonState,
    incoming: Option<RadioVirtualPlaylist>,
    queue_len: usize,
) -> Option<RadioVirtualPlaylist> {
    let mut radio = incoming?;
    let source_video_id = radio.source_video_id.clone();
    let incoming_selected_chip_index = radio.selected_chip_index;
    let current_chip_state = state
        .radio_virtual_playlist
        .as_ref()
        .filter(|current| current.source_video_id == source_video_id && !current.chips.is_empty())
        .map(|current| (current.chips.clone(), current.selected_chip_index));

    radio.chips.clear();
    radio.selected_chip_index = None;

    if let Some((chips, selected_chip_index)) = current_chip_state {
        radio.chips = chips;
        radio.selected_chip_index = selected_chip_index;
        normalize_radio_chip_selection(&mut radio);
        return Some(radio);
    }

    if queue_len > 1 {
        match ensure_client_loaded(state).and_then(|client| {
            client
                .get_radio(&source_video_id, None, None)
                .map(|radio| radio.chips)
        }) {
            Ok(chips) => {
                radio.chips = chips;
                radio.selected_chip_index = incoming_selected_chip_index;
                normalize_radio_chip_selection(&mut radio);
            }
            Err(err) => {
                eprintln!("[RADIO] failed to load daemon-owned chips for {source_video_id}: {err}");
            }
        }
    }

    Some(radio)
}

fn radio_chips_with_replacement(
    existing: &[RadioChip],
    replacement: Vec<RadioChip>,
) -> Vec<RadioChip> {
    if replacement.is_empty() {
        existing.to_vec()
    } else {
        replacement
    }
}

fn apply_radio_chips_from_refresh(
    radio: &mut RadioVirtualPlaylist,
    replacement: Vec<RadioChip>,
) -> bool {
    let selected_playlist_id = radio
        .selected_chip_index
        .and_then(|index| radio.chips.get(index))
        .map(|chip| chip.playlist_id.clone());
    let next = radio_chips_with_replacement(&radio.chips, replacement);
    let mut changed = false;
    if next != radio.chips {
        radio.chips = next;
        radio.selected_chip_index = selected_playlist_id.and_then(|playlist_id| {
            radio
                .chips
                .iter()
                .position(|chip| chip.playlist_id == playlist_id)
        });
        changed = true;
    }
    normalize_radio_chip_selection(radio) || changed
}

fn activate_radio_chip(state: &mut DaemonState, chip_index: usize) -> Result<()> {
    let radio = state
        .radio_virtual_playlist
        .clone()
        .ok_or_else(|| anyhow!("Radio chips only appear for radio queues"))?;
    let chip = radio
        .chips
        .get(chip_index)
        .cloned()
        .ok_or_else(|| anyhow!("That radio filter is no longer available"))?;
    let source_video_id = radio.source_video_id;
    let chips = radio.chips;
    let prefix_len = radio.touched_len.max(1).min(state.queue.len().max(1));
    let source_track = state
        .queue
        .iter()
        .find(|track| track.video_id == source_video_id)
        .cloned()
        .or_else(|| state.queue.first().cloned());

    let client = ensure_client_loaded(state)?;
    let mut refreshed = client.get_radio(
        &source_video_id,
        Some(&chip.playlist_id),
        chip.params.as_deref(),
    )?;

    if let Some(source_index) = refreshed
        .tracks
        .iter()
        .position(|track| track.video_id == source_video_id)
    {
        if source_index > 0 {
            let track = refreshed.tracks.remove(source_index);
            refreshed.tracks.insert(0, track);
        }
    } else if let Some(source_track) = source_track {
        refreshed.tracks.insert(0, source_track);
    }

    let mut queue = state.queue.clone();
    queue.truncate(prefix_len.min(queue.len()));
    queue.extend(dedupe_radio_recommendations(
        queue.as_slice(),
        refreshed.tracks,
    ));
    if queue.is_empty() {
        bail!("Radio filter returned no playable tracks");
    }

    state.queue = queue;
    state.queue_index = state.queue_index.min(state.queue.len().saturating_sub(1));
    state.radio_virtual_playlist = Some(RadioVirtualPlaylist {
        source_video_id,
        touched_len: prefix_len.min(state.queue.len()),
        // Chip activation responses may omit the original chip cloud. The daemon owns
        // the current chip set, so activating a chip preserves the existing controls.
        chips,
        selected_chip_index: Some(chip_index),
    });
    state.radio_append_in_flight = false;
    state.last_radio_append_anchor = None;
    prefetch_upcoming(state);
    sync_mpris(state);
    Ok(())
}

fn radio_queue_from_seed(
    state: &mut DaemonState,
    mut queue: Vec<Track>,
    seed_index: usize,
    prefix_len: usize,
) -> Result<(Vec<Track>, RadioVirtualPlaylist)> {
    if queue.is_empty() {
        bail!("Radio needs at least one queued track");
    }
    let seed_track = queue
        .get(seed_index)
        .cloned()
        .ok_or_else(|| anyhow!("Radio seed index {seed_index} is out of bounds"))?;
    let source_video_id = seed_track.video_id.clone();
    let prefix_len = prefix_len
        .max(seed_index.saturating_add(1))
        .min(queue.len());
    let mut chips = Vec::new();

    match ensure_client_loaded(state)
        .and_then(|client| client.get_radio(&source_video_id, None, None))
    {
        Ok(mut radio) => {
            if let Some(source_index) = radio
                .tracks
                .iter()
                .position(|track| track.video_id == source_video_id)
            {
                if source_index > 0 {
                    let track = radio.tracks.remove(source_index);
                    radio.tracks.insert(0, track);
                }
            } else {
                radio.tracks.insert(0, seed_track);
            }

            queue.truncate(prefix_len);
            queue.extend(dedupe_radio_recommendations(queue.as_slice(), radio.tracks));
            chips = radio.chips;
        }
        Err(err) => {
            eprintln!("[RADIO] failed to start radio from {source_video_id}: {err}");
        }
    }

    let mut radio_virtual_playlist = RadioVirtualPlaylist {
        source_video_id,
        touched_len: prefix_len.min(queue.len()),
        chips,
        selected_chip_index: None,
    };
    normalize_radio_chip_selection(&mut radio_virtual_playlist);
    Ok((queue, radio_virtual_playlist))
}

fn start_radio_from_seed(
    state: &mut DaemonState,
    queue: Vec<Track>,
    index: usize,
    seed_index: usize,
    prefix_len: usize,
    playlist_context: Option<PlaylistContext>,
    repeat_mode: RepeatMode,
    play: bool,
    suppress_reporting: bool,
) -> Result<()> {
    let (queue, radio_virtual_playlist) =
        radio_queue_from_seed(state, queue, seed_index, prefix_len)?;
    let index = index.min(queue.len().saturating_sub(1));

    if play {
        pause_other_mpris_players(state);
        return play_queue_at(
            state,
            queue,
            index,
            playlist_context,
            Some(radio_virtual_playlist),
            repeat_mode,
            PlaybackStartMode::Playing,
            suppress_reporting,
        );
    }

    state.queue = queue;
    state.queue_index = index;
    state.playlist_context = playlist_context;
    state.radio_virtual_playlist = Some(radio_virtual_playlist);
    state.radio_append_in_flight = false;
    state.last_radio_append_anchor = None;
    let repeat_mode_changed = state.repeat_mode != repeat_mode;
    state.repeat_mode = repeat_mode;
    if repeat_mode_changed {
        reset_track_repeat_budget_for_current_track(state);
    }
    prefetch_upcoming(state);
    sync_mpris(state);
    Ok(())
}

fn extend_radio_touched_for_insert(
    radio_virtual_playlist: &mut Option<RadioVirtualPlaylist>,
    insert_at: usize,
    added: usize,
) {
    if added == 0 {
        return;
    }

    if let Some(radio) = radio_virtual_playlist.as_mut() {
        let touched_len = radio.touched_len;
        let inserted_end = insert_at + added;
        if insert_at < touched_len {
            radio.touched_len = touched_len + added;
        } else {
            radio.touched_len = touched_len.max(inserted_end);
        }
    }
}

fn radio_touched_len_after_reorder(touched_len: usize, from: usize, insert_at: usize) -> usize {
    let mut touched_len = touched_len;

    if from < touched_len {
        touched_len = touched_len.saturating_sub(1);
    }

    if insert_at < touched_len {
        touched_len + 1
    } else {
        touched_len.max(insert_at.saturating_add(1))
    }
}

fn update_radio_touched_for_reorder(
    radio_virtual_playlist: &mut Option<RadioVirtualPlaylist>,
    from: usize,
    insert_at: usize,
) {
    if let Some(radio) = radio_virtual_playlist.as_mut() {
        radio.touched_len = radio_touched_len_after_reorder(radio.touched_len, from, insert_at);
    }
}

fn shrink_radio_touched_for_remove(
    radio_virtual_playlist: &mut Option<RadioVirtualPlaylist>,
    remove_at: usize,
) {
    if let Some(radio) = radio_virtual_playlist.as_mut() {
        if radio.touched_len == 0 {
            return;
        }

        if remove_at < radio.touched_len {
            radio.touched_len = radio.touched_len.saturating_sub(1);
        }
    }
}

fn adjusted_reorder_index(current: usize, from: usize, insert_at: usize) -> usize {
    if from == current {
        insert_at
    } else if from < current && insert_at >= current {
        current.saturating_sub(1)
    } else if from > current && insert_at <= current {
        current.saturating_add(1)
    } else {
        current
    }
}

fn insert_into_queue(state: &mut DaemonState, tracks: Vec<Track>, index: usize) {
    if tracks.is_empty() {
        return;
    }

    let had_queue = !state.queue.is_empty();
    let insert_at = index.min(state.queue.len());
    let added = tracks.len();
    for (offset, track) in tracks.into_iter().enumerate() {
        state.queue.insert(insert_at + offset, track);
    }

    if had_queue && insert_at <= state.queue_index {
        state.queue_index = state.queue_index.saturating_add(added);
    } else {
        state.queue_index = state.queue_index.min(state.queue.len().saturating_sub(1));
    }

    extend_radio_touched_for_insert(&mut state.radio_virtual_playlist, insert_at, added);
    state.radio_append_in_flight = false;
    state.last_radio_append_anchor = None;
    prefetch_upcoming(state);
    sync_mpris(state);
}

fn remove_from_queue(state: &mut DaemonState, index: usize) -> Result<Track> {
    if index >= state.queue.len() {
        bail!("Queue row is gone");
    }
    if index == state.queue_index {
        bail!("Remove a non-playing row or move the queue around it");
    }

    let removed = state.queue.remove(index);
    if index < state.queue_index {
        state.queue_index = state.queue_index.saturating_sub(1);
    } else {
        state.queue_index = state.queue_index.min(state.queue.len().saturating_sub(1));
    }
    shrink_radio_touched_for_remove(&mut state.radio_virtual_playlist, index);
    state.radio_append_in_flight = false;
    state.last_radio_append_anchor = None;
    prefetch_upcoming(state);
    sync_mpris(state);
    Ok(removed)
}

fn move_queue_item(state: &mut DaemonState, from: usize, raw_slot: usize) -> Result<Track> {
    let insert_at = if from < raw_slot {
        raw_slot.saturating_sub(1)
    } else {
        raw_slot
    };
    if from >= state.queue.len() || raw_slot > state.queue.len() {
        bail!("Queue row is gone");
    }
    if insert_at == from {
        return Ok(state.queue[from].clone());
    }

    let moved = state.queue.remove(from);
    state.queue.insert(insert_at, moved.clone());
    state.queue_index = adjusted_reorder_index(state.queue_index, from, insert_at)
        .min(state.queue.len().saturating_sub(1));
    update_radio_touched_for_reorder(&mut state.radio_virtual_playlist, from, insert_at);
    state.radio_append_in_flight = false;
    state.last_radio_append_anchor = None;
    prefetch_upcoming(state);
    sync_mpris(state);
    Ok(moved)
}

fn dismiss_queue_after_current(state: &mut DaemonState) -> bool {
    let keep_len = state.queue_index.saturating_add(1).min(state.queue.len());
    let mut changed = false;

    if state.queue.len() > keep_len {
        state.queue.truncate(keep_len);
        changed = true;
    }

    if state.radio_virtual_playlist.take().is_some() {
        changed = true;
    }

    if changed {
        state.queue_index = state.queue_index.min(state.queue.len().saturating_sub(1));
        state.radio_append_in_flight = false;
        state.last_radio_append_anchor = None;
        prefetch_upcoming(state);
        sync_mpris(state);
    }

    changed
}

fn handle_request(state: &mut DaemonState, request: DaemonRequest) -> DaemonResponse {
    match request {
        DaemonRequest::Ping => DaemonResponse::Pong,
        DaemonRequest::GetState => DaemonResponse::State(state_snapshot(state)),
        DaemonRequest::RateCurrentTrack { rating } => {
            let Some(video_id) = state
                .active_track
                .as_ref()
                .map(|track| track.video_id.clone())
            else {
                return DaemonResponse::Error("No song is currently playing".to_string());
            };

            match ensure_client_loaded(state)
                .and_then(|client| client.set_song_rating(&video_id, rating))
            {
                Ok(()) => {
                    if let Some(active_track) = state.active_track.as_mut() {
                        if active_track.video_id == video_id {
                            active_track.rating = rating;
                        }
                    }
                    maybe_persist_playback_state(state, true);
                    DaemonResponse::State(state_snapshot(state))
                }
                Err(err) => DaemonResponse::Error(err.to_string()),
            }
        }
        DaemonRequest::PlayQueueAt {
            queue,
            index,
            playlist_context,
            radio_virtual_playlist,
            repeat_mode,
            pause_other_mpris_players: should_pause_peer_mpris_players,
            start_paused,
            start_position_millis,
            suppress_reporting,
        } => {
            if should_pause_peer_mpris_players && !queue.is_empty() {
                pause_other_mpris_players(state);
            }
            let radio_virtual_playlist =
                daemon_owned_radio_virtual_playlist(state, radio_virtual_playlist, queue.len());
            let start_mode = if start_paused {
                PlaybackStartMode::Paused {
                    position_millis: start_position_millis,
                }
            } else {
                PlaybackStartMode::Playing
            };
            match play_queue_at(
                state,
                queue,
                index,
                playlist_context,
                radio_virtual_playlist,
                repeat_mode,
                start_mode,
                suppress_reporting || start_paused,
            ) {
                Ok(()) => {
                    maybe_persist_playback_state(state, true);
                    DaemonResponse::State(state_snapshot(state))
                }
                Err(err) => DaemonResponse::Error(err.to_string()),
            }
        }
        DaemonRequest::ReplaceQueue {
            queue,
            index,
            playlist_context,
            radio_virtual_playlist,
            repeat_mode,
        } => {
            let radio_virtual_playlist =
                daemon_owned_radio_virtual_playlist(state, radio_virtual_playlist, queue.len());
            state.queue = queue;
            state.queue_index = index.min(state.queue.len().saturating_sub(1));
            state.playlist_context = playlist_context;
            state.radio_virtual_playlist = radio_virtual_playlist;
            state.radio_append_in_flight = false;
            state.last_radio_append_anchor = None;
            let repeat_mode_changed = state.repeat_mode != repeat_mode;
            state.repeat_mode = repeat_mode;
            if repeat_mode_changed {
                reset_track_repeat_budget_for_current_track(state);
            }
            prefetch_upcoming(state);
            sync_mpris(state);
            maybe_persist_playback_state(state, true);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::StartRadioFromSeed {
            queue,
            index,
            seed_index,
            prefix_len,
            playlist_context,
            repeat_mode,
            play,
            suppress_reporting,
        } => match start_radio_from_seed(
            state,
            queue,
            index,
            seed_index,
            prefix_len,
            playlist_context,
            repeat_mode,
            play,
            suppress_reporting,
        ) {
            Ok(()) => {
                maybe_persist_playback_state(state, true);
                DaemonResponse::State(state_snapshot(state))
            }
            Err(err) => DaemonResponse::Error(err.to_string()),
        },
        DaemonRequest::InsertIntoQueue { tracks, index } => {
            insert_into_queue(state, tracks, index);
            maybe_persist_playback_state(state, true);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::RemoveFromQueue { index } => match remove_from_queue(state, index) {
            Ok(_) => {
                maybe_persist_playback_state(state, true);
                DaemonResponse::State(state_snapshot(state))
            }
            Err(err) => DaemonResponse::Error(err.to_string()),
        },
        DaemonRequest::MoveQueueItem { from, raw_slot } => {
            match move_queue_item(state, from, raw_slot) {
                Ok(_) => {
                    maybe_persist_playback_state(state, true);
                    DaemonResponse::State(state_snapshot(state))
                }
                Err(err) => DaemonResponse::Error(err.to_string()),
            }
        }
        DaemonRequest::DismissQueueAfterCurrent => {
            dismiss_queue_after_current(state);
            maybe_persist_playback_state(state, true);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::ProbeTrack { track } => {
            if state.probe_context.is_none() && state.active_track.is_some() {
                pause_other_mpris_players(state);
            }
            match probe_track(state, track) {
                Ok(()) => {
                    maybe_persist_playback_state(state, true);
                    DaemonResponse::State(state_snapshot(state))
                }
                Err(err) => DaemonResponse::Error(err.to_string()),
            }
        }
        DaemonRequest::TogglePause => {
            let result = if state.probe_context.is_some() {
                restore_after_probe(state, false)
            } else {
                toggle_pause_for_user_action(state);
                Ok(())
            };
            match result {
                Ok(()) => {
                    maybe_persist_playback_state(state, true);
                    DaemonResponse::State(state_snapshot(state))
                }
                Err(err) => DaemonResponse::Error(err.to_string()),
            }
        }
        DaemonRequest::Pause => {
            let result = if state.probe_context.is_some() {
                restore_after_probe(state, false)
            } else if cancel_pending_mpris_halt_auto_resume(state, "user pause") {
                Ok(())
            } else {
                state.peer_mpris.clear_waiting();
                clear_mpris_halt_auto_resume_pending(state);
                pause_local_playback(state, PlaybackTransition::Immediate);
                sync_mpris(state);
                Ok(())
            };
            match result {
                Ok(()) => {
                    maybe_persist_playback_state(state, true);
                    DaemonResponse::State(state_snapshot(state))
                }
                Err(err) => DaemonResponse::Error(err.to_string()),
            }
        }
        DaemonRequest::Resume => {
            if mpris_halt_auto_resume_is_pending(state) {
                resume_pending_mpris_halt_auto(state, PlaybackTransition::Immediate, "user resume");
            } else {
                resume_for_user_action(state);
            }
            maybe_persist_playback_state(state, true);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::CancelHaltAutoResume => {
            cancel_pending_mpris_halt_auto_resume(state, "UI request");
            maybe_persist_playback_state(state, true);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::ResumeHaltAutoResume => {
            if mpris_halt_auto_resume_is_pending(state) {
                resume_pending_mpris_halt_auto(state, PlaybackTransition::Immediate, "UI request");
            } else {
                resume_for_user_action(state);
            }
            maybe_persist_playback_state(state, true);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::Stop => {
            stop_playback(state, false);
            state.probe_context = None;
            sync_mpris(state);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::Next => match advance_queue_for_user_action(state, true) {
            Ok(()) => {
                maybe_persist_playback_state(state, true);
                DaemonResponse::State(state_snapshot(state))
            }
            Err(err) => DaemonResponse::Error(err.to_string()),
        },
        DaemonRequest::Previous => match handle_previous_for_user_action(state) {
            Ok(()) => {
                maybe_persist_playback_state(state, true);
                DaemonResponse::State(state_snapshot(state))
            }
            Err(err) => DaemonResponse::Error(err.to_string()),
        },
        DaemonRequest::SeekToMillis { position_millis } => {
            let target = ClockTime::from_mseconds(position_millis);
            seek_to_position(state, target);
            maybe_persist_playback_state(state, true);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::SetVolume { volume } => {
            set_logical_volume(state, volume);
            sync_mpris(state);
            maybe_persist_playback_state(state, true);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::SetRepeatMode { repeat_mode } => {
            state.repeat_mode = repeat_mode;
            reset_track_repeat_budget_for_current_track(state);
            sync_mpris(state);
            maybe_persist_playback_state(state, true);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::ActivateRadioChip { chip_index } => {
            match activate_radio_chip(state, chip_index) {
                Ok(()) => {
                    maybe_persist_playback_state(state, true);
                    DaemonResponse::State(state_snapshot(state))
                }
                Err(err) => DaemonResponse::Error(err.to_string()),
            }
        }
        DaemonRequest::SetSleepTimer { minutes } => {
            if minutes == 0 {
                return DaemonResponse::Error(
                    "Sleep timer minutes must be greater than zero".to_string(),
                );
            }
            let duration = Duration::from_secs(minutes as u64 * 60);
            let now = Instant::now();
            let Some(deadline) = now.checked_add(duration) else {
                return DaemonResponse::Error("Sleep timer is too far in the future".to_string());
            };
            state.sleep_deadline = Some(deadline);
            state.sleep_after_current = false;
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::SetSleepAfterCurrent => {
            if state.active_track.is_none() {
                return DaemonResponse::Error("No song is currently playing".to_string());
            }
            state.sleep_deadline = None;
            state.sleep_after_current = true;
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::ClearSleepTimer => {
            clear_sleep_state(state);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::GetSleepStatus => DaemonResponse::SleepStatus(sleep_status_snapshot(state)),
        DaemonRequest::RegisterUiClient { client_id } => {
            note_ui_client(state, client_id);
            DaemonResponse::Ok
        }
        DaemonRequest::UnregisterUiClient { client_id } => {
            remove_ui_client(state, &client_id);
            DaemonResponse::Ok
        }
        DaemonRequest::CopyShareUrl => match current_share_url(state) {
            Some(url) => DaemonResponse::ShareUrl(url),
            None => DaemonResponse::Error("No song is currently playing".to_string()),
        },
        DaemonRequest::LogMessage { source, message } => {
            match crate::logging::append_log(source, &message) {
                Ok(()) => DaemonResponse::Ok,
                Err(err) => DaemonResponse::Error(format!("Failed to write log: {err}")),
            }
        }
        DaemonRequest::GetCoordinationTargets => {
            DaemonResponse::CoordinationTargets(coordination_targets_snapshot(state))
        }
        DaemonRequest::ReloadSettings => {
            reload_settings(state);
            sync_mpris(state);
            maybe_persist_playback_state(state, true);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::Shutdown => {
            request_daemon_shutdown(state, true);
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
        BackgroundEvent::PrefetchFinished { video_id } => {
            state.prefetch_in_flight.remove(&video_id);
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

            let chips_changed = state
                .radio_virtual_playlist
                .as_mut()
                .is_some_and(|radio| apply_radio_chips_from_refresh(radio, chips));

            if added > 0 || chips_changed {
                sync_mpris(state);
                maybe_persist_playback_state(state, true);
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
    poll_pipewire_streams(state);
    maybe_unduck_for_pipewire(state);
    maybe_duck_for_pipewire(state);
    advance_volume_ramp(state);
    if state.shutting_down.get() {
        return;
    }
    if maybe_request_headless_sleep_shutdown(state, Instant::now()) {
        return;
    }

    if maybe_trigger_sleep_deadline(state, Instant::now()) {
        return;
    }

    if should_count_paused_idle_for_shutdown(
        state.active_track.is_some(),
        is_logically_paused(state),
        state.peer_mpris.is_waiting_for_peer_pause(),
        mpris_halt_auto_resume_is_pending(state),
        state.pipewire_pause_active,
    ) {
        if state.paused_since.is_none() {
            state.paused_since = Some(Instant::now());
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
        if !active.awaiting_fresh_position {
            if let Some(position) = state.player.position() {
                active.last_position_millis = position.mseconds();
            }
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
    maybe_sync_live_mpris_position(state);
    maybe_sync_pending_mpris_halt_auto_position(state);
    maybe_apply_sponsorblock_skip(state);
    maybe_run_deferred_action(state);
    maybe_extend_radio_queue(state);
    maybe_persist_playback_state(state, false);

    let reached_track_end = state.active_track.as_ref().is_some_and(|active| {
        has_reached_track_end(
            is_fading_out_to_pause(state),
            active.awaiting_fresh_position,
            state.player.position(),
            state.player.duration(),
        )
    });

    if reached_track_end {
        if let Some(resume_playback) = state
            .probe_context
            .as_ref()
            .map(|probe| probe.resume_playback)
        {
            if let Err(err) = restore_after_probe(state, resume_playback) {
                eprintln!("[DAEMON] failed to restore queue after probe playback completed: {err}");
                state.probe_context = None;
                stop_playback(state, false);
            }
            return;
        }

        if state.sleep_after_current {
            trigger_sleep_pause(state);
            return;
        }

        let repeat_mode = advance_repeat_mode_for_natural_end(state);
        let queue_len = effective_queue_len(
            state.queue.len(),
            repeat_mode,
            state
                .radio_virtual_playlist
                .as_ref()
                .map(|radio| radio.touched_len),
        );
        match natural_end_target_index(queue_len, state.queue_index, repeat_mode) {
            Some(target) => {
                let budget_mode = if repeat_mode == RepeatMode::One && target == state.queue_index {
                    TrackRepeatBudgetMode::PreserveCurrent
                } else {
                    TrackRepeatBudgetMode::ResetForCurrentTrack
                };
                if let Err(err) = play_index_with_startup_retries(
                    state,
                    target,
                    0,
                    budget_mode,
                    PlaybackStartMode::Playing,
                    false,
                ) {
                    eprintln!("[DAEMON] failed to auto-advance: {err}");
                }
            }
            None if should_wait_for_radio_extension_at_natural_end(
                state.queue.len(),
                state.queue_index,
                state.radio_virtual_playlist.is_some(),
                state.radio_append_in_flight,
            ) => {}
            None => stop_playback(state, false),
        }
    }
}

fn should_count_paused_idle_for_shutdown(
    has_active_track: bool,
    logically_paused: bool,
    mpris_peer_pause_active: bool,
    mpris_halt_auto_pending: bool,
    pipewire_pause_active: bool,
) -> bool {
    has_active_track
        && logically_paused
        && !mpris_peer_pause_active
        && !mpris_halt_auto_pending
        && !pipewire_pause_active
}

fn has_reached_track_end(
    pause_fade_pending: bool,
    awaiting_fresh_position: bool,
    position: Option<ClockTime>,
    duration: Option<ClockTime>,
) -> bool {
    !pause_fade_pending
        && !awaiting_fresh_position
        && matches!(
            (position, duration),
            (Some(position), Some(duration))
                if duration > ClockTime::from_seconds(1)
                    && position + ClockTime::from_mseconds(500) >= duration
        )
}

fn play_queue_at(
    state: &mut DaemonState,
    queue: Vec<Track>,
    index: usize,
    playlist_context: Option<PlaylistContext>,
    radio_virtual_playlist: Option<RadioVirtualPlaylist>,
    repeat_mode: RepeatMode,
    start_mode: PlaybackStartMode,
    suppress_reporting: bool,
) -> Result<()> {
    if queue.is_empty() {
        stop_playback(state, false);
        return Ok(());
    }
    state.queue = queue;
    state.queue_index = index.min(state.queue.len().saturating_sub(1));
    state.playlist_context = playlist_context;
    state.radio_virtual_playlist = radio_virtual_playlist;
    state.radio_append_in_flight = false;
    state.last_radio_append_anchor = None;
    state.repeat_mode = repeat_mode;
    state.track_repeat_loops_left = None;
    play_index_with_start_mode(state, state.queue_index, start_mode, suppress_reporting)
}

fn capture_probe_restore_state(state: &DaemonState) -> ProbeRestoreState {
    let queue_index = state.queue_index.min(state.queue.len().saturating_sub(1));
    let queue_video_id = state
        .queue
        .get(queue_index)
        .map(|track| track.video_id.clone());
    let position_millis = pause_anchor_position_millis(state);

    ProbeRestoreState {
        queue_index,
        queue_video_id,
        position_millis,
        resume_playback: state.active_track.is_some() && !is_logically_paused(state),
        track_repeat_loops_left: state.track_repeat_loops_left,
    }
}

fn resolve_probe_restore_index(state: &DaemonState, probe: &ProbeRestoreState) -> Option<usize> {
    if state.queue.is_empty() {
        return None;
    }

    let fallback_index = probe.queue_index.min(state.queue.len().saturating_sub(1));
    if state
        .queue
        .get(fallback_index)
        .is_some_and(|track| Some(track.video_id.as_str()) == probe.queue_video_id.as_deref())
    {
        return Some(fallback_index);
    }

    probe
        .queue_video_id
        .as_ref()
        .and_then(|video_id| {
            state
                .queue
                .iter()
                .position(|track| track.video_id == *video_id)
        })
        .or(Some(fallback_index))
}

fn suspend_playback_for_probe(state: &mut DaemonState) {
    clear_mpris_halt_auto_resume_pending(state);
    state.player.stop();
    state.volume_ramp = None;
    state.player = PlaybackEngine::new();
    state.active_track.take();
    state.track_repeat_loops_left = None;
    state.sponsor_segments.clear();
    state.sponsor_segments_reported.clear();
    state.paused_since = None;
    state.deferred_actions.clear();
    state.last_duration_micros.set(None);
    state.last_pipewire_poll_at = None;
    state.peer_mpris.clear_waiting();
}

fn start_probe_playback(
    state: &mut DaemonState,
    track: Track,
    prepared_uri: String,
    playback_source: PlaybackSource,
) -> Result<()> {
    eprintln!(
        "[DAEMON] starting probe playback for {} from {}",
        track.video_id,
        playback_source.label(),
    );
    state.player.set_volume(steady_output_volume(state));
    state.player.play_uri(&prepared_uri);
    state.last_duration_micros.set(None);
    let now = Instant::now();
    state.active_track = Some(ActiveTrack {
        video_id: track.video_id.clone(),
        title: track.title.clone(),
        artist: artist_string(&track),
        thumb_url: track.thumbnail_url.clone(),
        playback_source,
        playback_uri: prepared_uri,
        reporting_enabled: false,
        rating: SongRating::Indifferent,
        tracking: None,
        started_at: now,
        last_position_millis: 0,
        sponsorblock_auto_skip_offset_millis: 0,
        sponsorblock_skipped_segment_ids: HashSet::new(),
        startup_seek_millis: None,
        startup_retries: 0,
        awaiting_fresh_position: true,
    });
    state.sponsor_segments.clear();
    state.sponsor_segments_reported.clear();
    state.paused_since = None;
    refresh_sponsorblock_for_current_track(state);
    prefetch_upcoming(state);
    state.last_pipewire_poll_at = None;
    poll_pipewire_streams(state);
    maybe_unduck_for_pipewire(state);
    maybe_duck_for_pipewire(state);
    signal_mpris_seeked(state, ClockTime::ZERO);
    sync_mpris(state);
    maybe_persist_playback_state(state, true);
    Ok(())
}

fn probe_track(state: &mut DaemonState, mut track: Track) -> Result<()> {
    let prepared = prepare_playback(state, &track.video_id)?;
    apply_prepared_video_id_to_track(state, None, &mut track, &prepared.video_id);
    if state.probe_context.is_none() {
        let probe_restore = capture_probe_restore_state(state);
        maybe_persist_playback_state(state, true);
        state.probe_context = Some(probe_restore);
    }

    suspend_playback_for_probe(state);
    start_probe_playback(state, track, prepared.uri, prepared.source)
}

fn restore_after_probe(state: &mut DaemonState, resume_playback: bool) -> Result<()> {
    let Some(probe) = state.probe_context.clone() else {
        return Ok(());
    };

    stop_playback(state, false);
    state.probe_context = None;

    let Some(index) = resolve_probe_restore_index(state, &probe) else {
        maybe_persist_playback_state(state, true);
        sync_mpris(state);
        return Ok(());
    };

    play_index(state, index)?;
    state.track_repeat_loops_left = if state.repeat_mode == RepeatMode::One {
        probe.track_repeat_loops_left
    } else {
        None
    };

    if let Some(position_millis) = probe.position_millis.filter(|position| *position > 0) {
        seek_to_position(state, ClockTime::from_mseconds(position_millis));
    }

    if !resume_playback {
        pause_local_playback(state, PlaybackTransition::Immediate);
        sync_mpris(state);
    }

    maybe_persist_playback_state(state, true);
    Ok(())
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

fn should_wait_for_radio_extension_at_natural_end(
    queue_len: usize,
    queue_index: usize,
    has_radio_queue: bool,
    radio_append_in_flight: bool,
) -> bool {
    radio_append_in_flight && has_radio_queue && queue_len > 0 && queue_index + 1 == queue_len
}

fn hide_timing_for_track(track: &ActiveTrack) -> bool {
    track.awaiting_fresh_position && track.startup_seek_millis.is_none()
}

fn visible_position_millis(state: &DaemonState) -> Option<u64> {
    if let Some(position_millis) = pending_pause_position_millis(state) {
        return Some(position_millis);
    }

    let track = state.active_track.as_ref()?;
    if track.awaiting_fresh_position {
        track
            .startup_seek_millis
            .or_else(|| state.player.position().map(|position| position.mseconds()))
    } else {
        state.player.position().map(|position| position.mseconds())
    }
}

fn visible_duration_millis(state: &DaemonState) -> Option<u64> {
    let track = state.active_track.as_ref()?;
    (!hide_timing_for_track(track))
        .then(|| state.player.duration().map(|duration| duration.mseconds()))
        .flatten()
}

fn mpris_position_millis_for_track(
    track: &ActiveTrack,
    player_position_millis: Option<u64>,
    paused: bool,
) -> Option<u64> {
    if track.awaiting_fresh_position {
        track
            .startup_seek_millis
            .or_else(|| player_position_millis.filter(|millis| *millis > 0))
            .or_else(|| (!paused).then(|| track.started_at.elapsed().as_millis() as u64))
            .or(Some(0))
    } else {
        player_position_millis
    }
}

fn mpris_position_millis(state: &DaemonState) -> Option<u64> {
    let track = state.active_track.as_ref()?;
    let player_position_millis = pending_pause_position_millis(state)
        .or_else(|| state.player.position().map(|position| position.mseconds()));
    let duration_millis = state.player.duration().map(|duration| duration.mseconds());
    let position_millis =
        mpris_position_millis_for_track(track, player_position_millis, is_logically_paused(state));

    match (position_millis, duration_millis) {
        (Some(position_millis), Some(duration_millis)) => {
            Some(position_millis.min(duration_millis))
        }
        (position_millis, None) => position_millis,
        (None, Some(_)) => None,
    }
}

fn mpris_duration_millis(state: &DaemonState) -> Option<u64> {
    state.active_track.as_ref()?;
    state.player.duration().map(|duration| duration.mseconds())
}

fn should_sync_live_mpris_position(
    last_position_millis: Option<u64>,
    next_position_millis: Option<u64>,
    last_synced_at: Option<Instant>,
    now: Instant,
) -> bool {
    next_position_millis.is_some()
        && last_position_millis != next_position_millis
        && last_synced_at
            .map(|last_synced_at| {
                now.duration_since(last_synced_at) >= MPRIS_POSITION_SYNC_INTERVAL
            })
            .unwrap_or(true)
}

fn should_emit_live_mpris_seeked_hint(
    last_position_millis: Option<u64>,
    next_position_millis: Option<u64>,
) -> bool {
    matches!(
        (last_position_millis, next_position_millis),
        (Some(last_position_millis), Some(next_position_millis))
            if next_position_millis > last_position_millis
                && next_position_millis / 1_000 > last_position_millis / 1_000
    )
}

fn maybe_sync_live_mpris_position(state: &DaemonState) {
    if state.active_track.is_none() || is_logically_paused(state) {
        return;
    }

    let last_position_millis = state.last_mpris_position_millis.get();
    let next_position_millis = mpris_position_millis(state);
    let now = Instant::now();
    if !should_sync_live_mpris_position(
        last_position_millis,
        next_position_millis,
        state.last_mpris_position_sync_at.get(),
        now,
    ) {
        return;
    }

    let mpris = state.mpris.borrow();
    let Some(bridge) = mpris.as_ref() else {
        return;
    };
    bridge.sync_position(state);
    if should_emit_live_mpris_seeked_hint(last_position_millis, next_position_millis) {
        if let Some(position_millis) = next_position_millis {
            bridge.emit_seeked((position_millis * 1_000) as i64);
        }
    }
    note_mpris_position_sync(state, next_position_millis, now);
}

fn maybe_sync_pending_mpris_halt_auto_position(state: &DaemonState) {
    if !mpris_halt_auto_resume_sidechannel_active(state) {
        return;
    }

    let now = Instant::now();
    if state
        .last_mpris_position_sync_at
        .get()
        .is_some_and(|last_synced_at| {
            now.duration_since(last_synced_at) < MPRIS_PENDING_POSITION_SYNC_INTERVAL
        })
    {
        return;
    }

    let next_position_millis = mpris_position_millis(state);
    let mpris = state.mpris.borrow();
    let Some(bridge) = mpris.as_ref() else {
        return;
    };

    bridge.sync_position(state);
    if let Some(position_millis) = next_position_millis {
        bridge.emit_seeked((position_millis * 1_000) as i64);
    }
    note_mpris_position_sync(state, next_position_millis, now);
}

fn note_explicit_seek(state: &mut DaemonState, target_millis: u64) {
    let Some(track) = state.active_track.as_mut() else {
        return;
    };

    if track.awaiting_fresh_position {
        if target_millis > 0 {
            if track.startup_seek_millis != Some(target_millis) {
                eprintln!(
                    "[DAEMON] tracking explicit startup seek for {} at {} ms",
                    track.video_id, target_millis
                );
            }
            track.startup_seek_millis = Some(target_millis);
        } else {
            track.startup_seek_millis = None;
        }
    } else {
        track.startup_seek_millis = None;
    }

    if target_millis == 0 {
        track.sponsorblock_auto_skip_offset_millis = 0;
        track.sponsorblock_skipped_segment_ids.clear();
    }

    track.last_position_millis = target_millis;
}

fn seek_to_position(state: &mut DaemonState, target: ClockTime) {
    state.player.seek(target);
    note_explicit_seek(state, target.mseconds());
    signal_mpris_seeked(state, target);
    sync_mpris(state);
}

fn playback_start_stalled(started_at: Instant, awaiting_fresh_position: bool) -> bool {
    awaiting_fresh_position && started_at.elapsed() >= PLAYBACK_START_TIMEOUT
}

fn playback_start_attempt_total() -> u8 {
    PLAYBACK_START_RETRY_LIMIT.saturating_add(1)
}

fn playback_position_confirms_start(track: &ActiveTrack, position_millis: u64) -> bool {
    if track.startup_seek_millis.is_some_and(|target_millis| {
        position_millis.saturating_add(PLAYBACK_START_SEEK_CONFIRM_LEEWAY_MILLIS) >= target_millis
    }) {
        return true;
    }

    let session_elapsed_millis = track.started_at.elapsed().as_millis() as u64;

    (position_millis >= PLAYBACK_START_MIN_CONFIRMED_POSITION_MILLIS
        && position_millis <= PLAYBACK_STARTED_POSITION_THRESHOLD_MILLIS)
        || (position_millis <= session_elapsed_millis + 1_500
            && track.last_position_millis > 0
            && position_millis >= track.last_position_millis.saturating_add(200))
}

fn previous_action_position_millis_for_track(
    track: &ActiveTrack,
    player_position_millis: Option<u64>,
    pending_pause_position_millis: Option<u64>,
) -> u64 {
    let raw_position_millis = if let Some(position_millis) = pending_pause_position_millis {
        position_millis
    } else if track.awaiting_fresh_position {
        track.startup_seek_millis.unwrap_or(0)
    } else {
        player_position_millis.unwrap_or(track.last_position_millis)
    };

    raw_position_millis.saturating_sub(track.sponsorblock_auto_skip_offset_millis)
}

fn previous_action_position_millis(state: &DaemonState) -> Option<u64> {
    let track = state.active_track.as_ref()?;
    let player_position_millis = state.player.position().map(|position| position.mseconds());
    Some(previous_action_position_millis_for_track(
        track,
        player_position_millis,
        pending_pause_position_millis(state),
    ))
}

fn pending_pause_position_millis(state: &DaemonState) -> Option<u64> {
    state.volume_ramp.and_then(|ramp| match ramp.completion {
        VolumeRampCompletion::PauseAtPosition { position_millis } => Some(position_millis),
        VolumeRampCompletion::None => None,
    })
}

fn is_fading_out_to_pause(state: &DaemonState) -> bool {
    pending_pause_position_millis(state).is_some() && !state.player.is_paused()
}

fn is_logically_paused(state: &DaemonState) -> bool {
    state.player.is_paused() || is_fading_out_to_pause(state)
}

fn pause_request_already_satisfied(
    player_paused: bool,
    pause_fade_pending: bool,
    transition: PlaybackTransition,
) -> bool {
    player_paused || (pause_fade_pending && transition == PlaybackTransition::Calm)
}

fn pause_anchor_position_millis(state: &DaemonState) -> Option<u64> {
    pending_pause_position_millis(state).or_else(|| {
        state.active_track.as_ref().map(|track| {
            track.startup_seek_millis.unwrap_or_else(|| {
                current_listened_millis(
                    state
                        .player
                        .position()
                        .map(|position| position.nseconds() / 1_000_000),
                    track,
                )
            })
        })
    })
}

fn pause_local_playback(state: &mut DaemonState, transition: PlaybackTransition) {
    if pause_request_already_satisfied(
        state.player.is_paused(),
        is_fading_out_to_pause(state),
        transition,
    ) {
        return;
    }
    let Some(position_millis) = pause_anchor_position_millis(state) else {
        return;
    };

    if let Some(track) = state.active_track.as_mut() {
        track.last_position_millis = position_millis;
    }

    match transition {
        PlaybackTransition::Immediate => {
            state.volume_ramp = None;
            state.player.pause();
            state.player.seek(ClockTime::from_mseconds(position_millis));
        }
        PlaybackTransition::Calm => start_pause_fade(state, position_millis),
    }
}

fn resume_local_playback(state: &mut DaemonState, transition: PlaybackTransition) {
    if !is_logically_paused(state) {
        return;
    }

    state.sleep_shutdown_pending = false;

    match transition {
        PlaybackTransition::Immediate => {
            state.volume_ramp = None;
            state.player.set_volume(steady_output_volume(state));
            if state.player.is_paused() {
                state.player.resume();
            }
        }
        PlaybackTransition::Calm => {
            if state.player.is_paused() {
                state.player.set_volume(0.0);
                state.player.resume();
            }
            start_playback_volume_ramp(state, steady_output_volume(state));
        }
    }
}

fn maybe_recover_stalled_playback_start(state: &mut DaemonState) -> bool {
    if is_logically_paused(state) {
        return false;
    }

    let Some(active) = state.active_track.as_ref() else {
        return false;
    };

    if !playback_start_stalled(active.started_at, active.awaiting_fresh_position) {
        return false;
    }

    let video_id = active.video_id.clone();
    let playback_source = active.playback_source;
    let queue_index = state.queue_index;
    let startup_retries = active.startup_retries;
    let startup_attempt = startup_retries.saturating_add(1);
    let elapsed_secs = active.started_at.elapsed().as_secs_f32();
    let buffered_percent = state.player.buffered_fraction() * 100.0;
    let pipeline_state = state.player.pipeline_state();

    if let Some(resume_playback) = state
        .probe_context
        .as_ref()
        .map(|probe| probe.resume_playback)
    {
        eprintln!(
            "[DAEMON] probe playback start stalled for {video_id} from {} on startup attempt {startup_attempt}/{} after {elapsed_secs:.1}s (buffered {buffered_percent:.0}%, pipeline {pipeline_state:?}); restoring queued track",
            playback_source.label(),
            playback_start_attempt_total(),
        );
        if let Err(err) = restore_after_probe(state, resume_playback) {
            eprintln!("[DAEMON] failed to restore queue after stalled probe playback: {err}");
            state.probe_context = None;
            stop_playback(state, false);
        }
        return true;
    }

    if startup_retries < PLAYBACK_START_RETRY_LIMIT {
        let next_retry = startup_retries.saturating_add(1);
        let next_attempt = next_retry.saturating_add(1);
        eprintln!(
            "[DAEMON] playback start stalled for {video_id} from {} on startup attempt {startup_attempt}/{} after {elapsed_secs:.1}s (buffered {buffered_percent:.0}%, pipeline {pipeline_state:?}); retrying ({next_attempt}/{})",
            playback_source.label(),
            playback_start_attempt_total(),
            playback_start_attempt_total(),
        );

        if should_evict_cache_after_startup_stall(playback_source) {
            let preserved_track_repeat_loops_left = state.track_repeat_loops_left;
            stop_playback(state, false);
            state.track_repeat_loops_left = preserved_track_repeat_loops_left;

            match playback_source {
                PlaybackSource::TempPrefetch => {
                    eprintln!(
                        "[DAEMON] evicting prefetched temp audio for {video_id} before startup retry"
                    );
                    state.cache.remove_temp_audio(&video_id);
                }
                PlaybackSource::OfflineCache => {
                    eprintln!(
                        "[DAEMON] evicting cached offline audio for {video_id} after startup stall before retry"
                    );
                    if let Err(err) = state.cache.remove_cached(&video_id) {
                        eprintln!(
                            "[DAEMON] failed to evict stalled offline cache for {video_id}: {err}"
                        );
                    }
                }
                PlaybackSource::RemoteStream => {}
            }
        }

        if let Err(err) = play_index_with_startup_retries(
            state,
            queue_index,
            next_retry,
            TrackRepeatBudgetMode::PreserveCurrent,
            PlaybackStartMode::Playing,
            false,
        ) {
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
    let position_millis = previous_action_position_millis(state);
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
            if state.probe_context.is_some() {
                seek_to_position(state, ClockTime::ZERO);
                Ok(())
            } else {
                restart_current_track(state)
            }
        }
        PreviousAction::PlayIndex(index) => play_index(state, index),
        PreviousAction::Noop => Ok(()),
    }
}

fn handle_previous_for_user_action(state: &mut DaemonState) -> Result<()> {
    let position_millis = previous_action_position_millis(state);
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
    if resume_pending_mpris_halt_auto(state, PlaybackTransition::Immediate, "UI play/pause") {
        return;
    }

    if is_logically_paused(state) && state.active_track.is_some() {
        pause_other_mpris_players(state);
    } else {
        state.peer_mpris.clear_waiting();
        clear_mpris_halt_auto_resume_pending(state);
    }
    if is_logically_paused(state) {
        resume_local_playback(state, PlaybackTransition::Immediate);
    } else {
        pause_local_playback(state, PlaybackTransition::Immediate);
    }
    sync_mpris(state);
}

fn resume_for_user_action(state: &mut DaemonState) {
    if resume_pending_mpris_halt_auto(state, PlaybackTransition::Immediate, "user resume") {
        return;
    }

    state.peer_mpris.clear_waiting();
    clear_mpris_halt_auto_resume_pending(state);
    if state.active_track.is_some() {
        pause_other_mpris_players(state);
    }
    resume_local_playback(state, PlaybackTransition::Immediate);
    sync_mpris(state);
}

fn play_index(state: &mut DaemonState, index: usize) -> Result<()> {
    play_index_with_reporting(state, index, false)
}

fn play_index_with_reporting(
    state: &mut DaemonState,
    index: usize,
    suppress_reporting: bool,
) -> Result<()> {
    play_index_with_start_mode(state, index, PlaybackStartMode::Playing, suppress_reporting)
}

fn play_index_with_start_mode(
    state: &mut DaemonState,
    index: usize,
    start_mode: PlaybackStartMode,
    suppress_reporting: bool,
) -> Result<()> {
    play_index_with_startup_retries(
        state,
        index,
        0,
        TrackRepeatBudgetMode::ResetForCurrentTrack,
        start_mode,
        suppress_reporting,
    )
}

fn play_index_with_startup_retries(
    state: &mut DaemonState,
    index: usize,
    startup_retries: u8,
    budget_mode: TrackRepeatBudgetMode,
    start_mode: PlaybackStartMode,
    suppress_reporting: bool,
) -> Result<()> {
    let preserved_track_repeat_loops_left = match budget_mode {
        TrackRepeatBudgetMode::ResetForCurrentTrack => None,
        TrackRepeatBudgetMode::PreserveCurrent => state.track_repeat_loops_left,
    };
    let track = state
        .queue
        .get(index)
        .cloned()
        .ok_or_else(|| anyhow!("Queue index {index} is out of bounds"))?;
    let prepared = prepare_playback(state, &track.video_id)?;

    stop_playback(state, false);
    state.player = PlaybackEngine::new();
    state.probe_context = None;

    start_prepared_playback(
        state,
        index,
        track,
        prepared,
        startup_retries,
        budget_mode,
        start_mode,
        suppress_reporting,
        preserved_track_repeat_loops_left,
    )
}

fn restart_current_track(state: &mut DaemonState) -> Result<()> {
    let Some(active) = state.active_track.as_ref() else {
        return Ok(());
    };
    let index = state.queue_index;
    let Some(track) = state.queue.get(index).cloned() else {
        seek_to_position(state, ClockTime::ZERO);
        return Ok(());
    };
    let prepared = PreparedPlayback {
        uri: active.playback_uri.clone(),
        source: active.playback_source,
        video_id: active.video_id.clone(),
    };
    let suppress_reporting = !active.reporting_enabled;
    let preserved_track_repeat_loops_left = state.track_repeat_loops_left;

    stop_playback(state, false);
    state.player = PlaybackEngine::new();
    state.probe_context = None;

    start_prepared_playback(
        state,
        index,
        track,
        prepared,
        0,
        TrackRepeatBudgetMode::PreserveCurrent,
        PlaybackStartMode::Playing,
        suppress_reporting,
        preserved_track_repeat_loops_left,
    )
}

fn start_prepared_playback(
    state: &mut DaemonState,
    index: usize,
    mut track: Track,
    prepared: PreparedPlayback,
    startup_retries: u8,
    budget_mode: TrackRepeatBudgetMode,
    start_mode: PlaybackStartMode,
    suppress_reporting: bool,
    preserved_track_repeat_loops_left: Option<u32>,
) -> Result<()> {
    let PreparedPlayback {
        uri: playback_uri,
        source: playback_source,
        video_id: prepared_video_id,
    } = prepared;
    apply_prepared_video_id_to_track(state, Some(index), &mut track, &prepared_video_id);
    let startup_attempt = startup_retries.saturating_add(1);
    state.queue_index = index;
    mark_radio_touched_through(state, index);
    eprintln!(
        "[DAEMON] starting playback for {} from {} (startup attempt {startup_attempt}/{})",
        track.video_id,
        playback_source.label(),
        playback_start_attempt_total(),
    );
    let paused_start_position_millis = match start_mode {
        PlaybackStartMode::Playing => None,
        PlaybackStartMode::Paused { position_millis } => position_millis,
    };
    let start_paused = matches!(start_mode, PlaybackStartMode::Paused { .. });
    state.player.set_volume(if start_paused {
        0.0
    } else {
        steady_output_volume(state)
    });
    state.player.play_uri(&playback_uri);
    if start_paused {
        state.player.pause();
        if let Some(position_millis) = paused_start_position_millis {
            state.player.seek(ClockTime::from_mseconds(position_millis));
        }
    }
    state.last_duration_micros.set(None);
    let now = Instant::now();
    state.active_track = Some(ActiveTrack {
        video_id: track.video_id.clone(),
        title: track.title.clone(),
        artist: artist_string(&track),
        thumb_url: track.thumbnail_url.clone(),
        playback_source,
        playback_uri,
        reporting_enabled: !suppress_reporting,
        rating: SongRating::Indifferent,
        tracking: None,
        started_at: now,
        last_position_millis: paused_start_position_millis.unwrap_or(0),
        sponsorblock_auto_skip_offset_millis: 0,
        sponsorblock_skipped_segment_ids: HashSet::new(),
        startup_seek_millis: paused_start_position_millis,
        startup_retries,
        awaiting_fresh_position: true,
    });
    match budget_mode {
        TrackRepeatBudgetMode::ResetForCurrentTrack => {
            reset_track_repeat_budget_for_current_track(state)
        }
        TrackRepeatBudgetMode::PreserveCurrent => {
            state.track_repeat_loops_left = preserved_track_repeat_loops_left;
        }
    }
    state.sponsor_segments.clear();
    state.sponsor_segments_reported.clear();
    state.paused_since = None;
    if suppress_reporting {
        eprintln!(
            "[TRACKING] suppressing telemetry bootstrap for restored playback of {}",
            track.video_id
        );
    } else {
        state
            .deferred_actions
            .push_back(DeferredAction::StartPlaybackTracking {
                video_id: track.video_id.clone(),
                attempt: 0,
                not_before: Instant::now(),
            });
    }
    refresh_sponsorblock_for_current_track(state);
    prefetch_upcoming(state);
    state.last_pipewire_poll_at = None;
    poll_pipewire_streams(state);
    maybe_unduck_for_pipewire(state);
    maybe_duck_for_pipewire(state);
    signal_mpris_seeked(state, ClockTime::ZERO);
    sync_mpris(state);
    maybe_persist_playback_state(state, true);
    Ok(())
}

fn mark_radio_touched_through(state: &mut DaemonState, index: usize) {
    if let Some(radio) = state.radio_virtual_playlist.as_mut() {
        radio.touched_len = radio.touched_len.max(index.saturating_add(1));
    }
}

fn stop_playback(state: &mut DaemonState, exiting: bool) {
    maybe_persist_playback_state(state, true);
    clear_mpris_halt_auto_resume_pending(state);
    if let Some(track) = state.active_track.take() {
        let current_position_millis = pending_pause_position_millis(state).or_else(|| {
            state
                .player
                .position()
                .map(|position| position.nseconds() / 1_000_000)
        });
        flush_active_track(track, current_position_millis, exiting);
    }
    state.player.stop();
    state.volume_ramp = None;
    state.track_repeat_loops_left = None;
    state.sponsor_segments.clear();
    state.sponsor_segments_reported.clear();
    state.paused_since = None;
    state.peer_mpris.clear_waiting();
    sync_mpris(state);
}

fn apply_prepared_video_id_to_track(
    state: &mut DaemonState,
    queue_index: Option<usize>,
    track: &mut Track,
    prepared_video_id: &str,
) {
    if track.video_id == prepared_video_id {
        return;
    }

    let old_video_id = std::mem::replace(&mut track.video_id, prepared_video_id.to_string());
    if let Some(index) = queue_index {
        if let Some(queue_track) = state.queue.get_mut(index) {
            if queue_track.video_id == old_video_id {
                queue_track.video_id = prepared_video_id.to_string();
            }
        }
    }

    let artist = artist_string(track);
    if let Err(err) = state.cache.upsert_track_metadata(
        prepared_video_id,
        &track.title,
        &artist,
        history_thumbnail_candidate(track.thumbnail_url.as_deref()),
    ) {
        eprintln!(
            "[PLAYER] failed to persist canonical metadata for {old_video_id} -> {prepared_video_id}: {err}"
        );
    }
    eprintln!("[PLAYER] using canonical video id {prepared_video_id} for stale id {old_video_id}");
}

fn prepare_playback(state: &mut DaemonState, video_id: &str) -> Result<PreparedPlayback> {
    if state.settings.prefer_local_cache_on_playback {
        if let Some(cached_path) = state.cache.get_cached_audio(video_id) {
            return url::Url::from_file_path(cached_path)
                .map(|url| PreparedPlayback {
                    uri: url.to_string(),
                    source: PlaybackSource::OfflineCache,
                    video_id: video_id.to_string(),
                })
                .map_err(|_| anyhow!("Failed to build file URL for cached audio"));
        }
        if state.settings.prefetch_enabled {
            if let Some(temp_path) = state.cache.get_temp_audio(video_id) {
                return url::Url::from_file_path(temp_path)
                    .map(|url| PreparedPlayback {
                        uri: url.to_string(),
                        source: PlaybackSource::TempPrefetch,
                        video_id: video_id.to_string(),
                    })
                    .map_err(|_| anyhow!("Failed to build file URL for prefetched audio"));
            }
        }
    }

    let client = ensure_client_loaded(state)?;
    let resolved = client
        .resolved_audio_stream_url(video_id)
        .map_err(|err| anyhow!("Playback failed: {err}"))?;
    if resolved.video_id != video_id {
        if let Err(err) = state.cache.replace_video_id(video_id, &resolved.video_id) {
            eprintln!(
                "[PLAYER] failed to migrate stale video id {video_id} -> {}: {err}",
                resolved.video_id
            );
        }
    }
    Ok(PreparedPlayback {
        uri: resolved.url,
        source: PlaybackSource::RemoteStream,
        video_id: resolved.video_id,
    })
}

fn should_retry_after_offline_cache_failure(
    playback_source: PlaybackSource,
    awaiting_fresh_position: bool,
) -> bool {
    playback_source == PlaybackSource::OfflineCache && awaiting_fresh_position
}

fn should_evict_cache_after_startup_stall(playback_source: PlaybackSource) -> bool {
    matches!(
        playback_source,
        PlaybackSource::OfflineCache | PlaybackSource::TempPrefetch
    )
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
    let awaiting_fresh_position = active.awaiting_fresh_position;
    let startup_attempt = active.startup_retries.saturating_add(1);

    if let Some(resume_playback) = state
        .probe_context
        .as_ref()
        .map(|probe| probe.resume_playback)
    {
        eprintln!(
            "[DAEMON] probe playback failed for {video_id} from {} (attempt {startup_attempt}/{}): {error}; restoring queued track",
            playback_source.label(),
            playback_start_attempt_total(),
        );
        if let Err(err) = restore_after_probe(state, resume_playback) {
            eprintln!("[DAEMON] failed to restore queue after probe playback error: {err}");
            state.probe_context = None;
            stop_playback(state, false);
        }
        return;
    }

    if awaiting_fresh_position {
        eprintln!(
            "[DAEMON] playback failed for {video_id} from {} before startup was confirmed (attempt {startup_attempt}/{}): {error}",
            playback_source.label(),
            playback_start_attempt_total(),
        );
    } else {
        eprintln!(
            "[DAEMON] playback failed for {video_id} from {} after startup was confirmed: {error}",
            playback_source.label(),
        );
    }

    if playback_source == PlaybackSource::TempPrefetch {
        eprintln!("[DAEMON] removing prefetched temp audio for {video_id} and retrying");
        stop_playback(state, false);
        state.cache.remove_temp_audio(&video_id);
        if let Err(err) = play_index(state, queue_index) {
            eprintln!("[DAEMON] retry after temp-cache eviction failed for {video_id}: {err}");
        }
        return;
    }

    if should_retry_after_offline_cache_failure(playback_source, awaiting_fresh_position) {
        eprintln!(
            "[DAEMON] evicting cached offline audio for {video_id} after startup failure and retrying"
        );
        if let Err(err) = state.cache.remove_cached(&video_id) {
            eprintln!("[DAEMON] failed to evict offline cache for {video_id}: {err}");
        }
        if let Err(err) = play_index(state, queue_index) {
            eprintln!("[DAEMON] retry after offline-cache eviction failed for {video_id}: {err}");
            stop_playback(state, false);
        }
        return;
    }

    if let Some(target) = queue_target_index(state, true) {
        if target != queue_index {
            if awaiting_fresh_position {
                eprintln!(
                    "[DAEMON] skipping failed track {video_id} after startup never confirmed from {}",
                    playback_source.label()
                );
            } else {
                eprintln!(
                    "[DAEMON] skipping failed track {video_id} after failure from {}",
                    playback_source.label()
                );
            }
            if let Err(err) = play_index(state, target) {
                eprintln!("[DAEMON] failed to advance after playback failure on {video_id}: {err}");
                stop_playback(state, false);
            }
            return;
        }
    }

    stop_playback(state, false);
}

fn cookie_fingerprint(cookie: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    cookie.hash(&mut hasher);
    hasher.finish()
}

fn ensure_client_loaded(state: &mut DaemonState) -> Result<Rc<NetworkState>> {
    let cookie = get_cookie().ok_or_else(|| anyhow!("No saved cookie available"))?;
    let fingerprint = cookie_fingerprint(&cookie);

    if let Some(client) = state.client.as_ref() {
        if state.client_cookie_fingerprint == Some(fingerprint) {
            return Ok(client.clone());
        }

        eprintln!("[AUTH] saved cookie changed; refreshing daemon auth client");
        state.client = None;
        state.client_cookie_fingerprint = None;
    }

    let client = Rc::new(NetworkState::from_cookie(&cookie)?);
    state.client = Some(client.clone());
    state.client_cookie_fingerprint = Some(fingerprint);
    Ok(client)
}

fn reload_settings(state: &mut DaemonState) {
    match state.cache.load_settings() {
        Ok(settings) => {
            state.settings = settings;
            reset_track_repeat_budget_for_current_track(state);
            if !state.settings.other_mpris_mode.coordinates_peers() {
                state.peer_mpris.deactivate();
            }
            state.last_pipewire_poll_at = None;
            poll_pipewire_streams(state);
            maybe_unduck_for_pipewire(state);
            maybe_duck_for_pipewire(state);
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
    _vr_refresh_token: Option<&str>,
    storage_root: Option<&str>,
) -> Result<()> {
    let mut cache = MusicCache::new_with_storage_root(storage_root)
        .context("Failed to initialise music cache")?;

    let client = cookie.and_then(|cookie| match NetworkState::from_cookie(cookie) {
        Ok(client) => Some(client),
        Err(err) => {
            eprintln!("[OFFLINE_SYNC] failed to initialize network state: {err}");
            None
        }
    });

    sync_downloaded_playlists(&mut cache, settings, client.as_ref());
    sync_auto_offline_cache(&mut cache, settings, client.as_ref());

    Ok(())
}

fn sync_downloaded_playlists(
    cache: &mut MusicCache,
    settings: &AppSettings,
    client: Option<&NetworkState>,
) {
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

        let auto_retained_ids = auto_offline_retention_ids(cache, settings);
        for video_id in removed_ids {
            remove_cached_if_unwanted_after_playlist_sync(cache, &video_id, &auto_retained_ids);
        }

        eprintln!("[PLAYLIST_SYNC] refreshed downloaded playlist: {playlist_id}");
    }
}

fn auto_offline_retention_ids(cache: &MusicCache, settings: &AppSettings) -> HashSet<String> {
    match cache.auto_offline_candidates(settings, settings.offline_cache_target as usize) {
        Ok(candidates) => candidates.into_iter().collect(),
        Err(err) => {
            eprintln!("[CACHE] failed to compute offline retention candidates: {err}");
            HashSet::new()
        }
    }
}

fn remove_cached_if_unwanted_after_playlist_sync(
    cache: &MusicCache,
    video_id: &str,
    auto_retained_ids: &HashSet<String>,
) {
    let still_in_playlist = cache
        .is_track_in_any_offline_playlist(video_id)
        .unwrap_or(false);
    if still_in_playlist {
        return;
    }

    if auto_retained_ids.contains(video_id) {
        if let Err(err) = cache.unpin_offline(video_id) {
            eprintln!("[PLAYLIST_SYNC] failed to unpin auto-retained {video_id}: {err}");
        }
        eprintln!(
            "[PLAYLIST_SYNC] kept removed track {video_id} because auto-cache still wants it"
        );
        return;
    }

    let _ = cache.remove_cached(video_id);
}

fn sync_auto_offline_cache(
    cache: &mut MusicCache,
    settings: &AppSettings,
    client: Option<&NetworkState>,
) {
    let cache_target = settings.offline_cache_target as usize;
    let top = if !settings.auto_manage_offline_cache && !settings.auto_cache_listen_again {
        Vec::new()
    } else {
        match cache.auto_offline_candidates(settings, cache_target) {
            Ok(candidates) => candidates,
            Err(err) => {
                eprintln!("[CACHE] failed to compute offline candidates: {err}");
                return;
            }
        }
    };
    let cached = cache.cached_ids().unwrap_or_default();
    let pinned = cache.pinned_ids().unwrap_or_default();

    for id in &cached {
        if !top.contains(id) && !pinned.contains(id) {
            let _ = cache.remove_cached(id);
        }
    }

    let to_download: Vec<String> = top.into_iter().filter(|id| !cached.contains(id)).collect();

    if to_download.is_empty() {
        return;
    }

    let Some(client) = client else {
        return;
    };

    for video_id in to_download {
        eprintln!("[CACHE] downloading {video_id} for offline...");
        let download = match download_audio_for_cache(cache, client, &video_id, "CACHE") {
            Ok(download) => {
                eprintln!(
                    "[CACHE] {} downloaded {} KB",
                    download.video_id,
                    download.audio.len() / 1024
                );
                download
            }
            Err(err) => {
                eprintln!("[CACHE] download failed for {video_id}: {err}");
                continue;
            }
        };

        let thumb_url = cache
            .get_thumbnail_url(&download.video_id)
            .or_else(|| cache.get_thumbnail_url(&video_id));
        let art = thumb_url.as_deref().and_then(fetch_thumbnail_bytes);

        let _ = cache.store_audio(&download.video_id, &download.audio, art.as_deref());
        hydrate_offline_sponsorblock_cache(cache, &download.video_id);
    }
}

fn ensure_track_available_offline(
    cache: &mut MusicCache,
    client: &NetworkState,
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

    let download = match download_audio_for_cache(cache, client, video_id, log_prefix) {
        Ok(download) => download,
        Err(err) => {
            eprintln!("[{log_prefix}] download failed for {video_id}: {err}");
            return false;
        }
    };

    let art = track
        .thumbnail_url
        .as_ref()
        .and_then(|url| fetch_thumbnail_bytes(url));

    if download.video_id != video_id.as_str() {
        let artist = artist_string(track);
        let _ = cache.upsert_track_metadata(
            &download.video_id,
            &track.title,
            &artist,
            history_thumbnail_candidate(track.thumbnail_url.as_deref()),
        );
    }
    let _ = cache.store_audio(&download.video_id, &download.audio, art.as_deref());
    if pin {
        let _ = cache.pin_offline(&download.video_id);
    }
    hydrate_offline_sponsorblock_cache(cache, &download.video_id);
    true
}

fn cache_media_download_retry_delay(failed_attempt: usize) -> Duration {
    match failed_attempt {
        1 => Duration::from_millis(350),
        _ => Duration::from_millis(1_000),
    }
}

fn download_audio_for_cache(
    cache: &MusicCache,
    client: &NetworkState,
    video_id: &str,
    log_prefix: &str,
) -> Result<CacheAudioDownload> {
    let mut video_id = video_id.to_string();
    let mut last_error = None;
    let mut saw_media_download_failure = false;

    for attempt in 1..=CACHE_MEDIA_DOWNLOAD_ATTEMPTS {
        let resolved = client
            .resolved_audio_stream_url(&video_id)
            .with_context(|| format!("failed to get audio URL for {video_id}"))?;
        if resolved.video_id != video_id {
            cache
                .replace_video_id(&video_id, &resolved.video_id)
                .with_context(|| {
                    format!(
                        "failed to migrate stale video id {video_id} -> {}",
                        resolved.video_id
                    )
                })?;
            video_id = resolved.video_id.clone();
        }
        let resolved_video_id = resolved.video_id;
        let attempt_result = download_media_bytes(&resolved.url, Some("bytes=0-"))
            .with_context(|| format!("failed to download media for {resolved_video_id}"))
            .map(|audio| CacheAudioDownload {
                video_id: resolved_video_id,
                audio,
            });

        match attempt_result {
            Ok(download) => return Ok(download),
            Err(err) => {
                let message = format!("{err:#}");
                saw_media_download_failure = true;
                if attempt < CACHE_MEDIA_DOWNLOAD_ATTEMPTS {
                    eprintln!(
                        "[{log_prefix}] download attempt {attempt}/{CACHE_MEDIA_DOWNLOAD_ATTEMPTS} failed for {video_id}: {message}; retrying with a fresh URL"
                    );
                    std::thread::sleep(cache_media_download_retry_delay(attempt));
                }
                last_error = Some(err);
            }
        }
    }

    if saw_media_download_failure {
        eprintln!(
            "[{log_prefix}] WEB_REMIX media download failed for {video_id}; trying Android VR cache fallback"
        );
        let fallback_video_id = video_id.clone();
        return client
            .android_vr_audio_stream_url(&video_id)
            .with_context(|| format!("failed to get Android VR audio URL for {video_id}"))
            .and_then(|url| {
                download_media_bytes(&url, Some("bytes=0-"))
                    .with_context(|| format!("failed to download Android VR media for {video_id}"))
            })
            .map(|audio| CacheAudioDownload {
                video_id: fallback_video_id,
                audio,
            })
            .or_else(|fallback_err| {
                if let Some(original_err) = last_error {
                    Err(original_err.context(format!(
                        "Android VR fallback also failed for {video_id}: {fallback_err:#}"
                    )))
                } else {
                    Err(fallback_err)
                }
            });
    }

    Err(last_error.unwrap_or_else(|| anyhow!("download attempts were not run for {video_id}")))
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
    let session_started_at = state
        .active_track
        .as_ref()
        .filter(|track| track.video_id == video_id)
        .map(|track| track.started_at)
        .unwrap_or_else(Instant::now);

    let client = match ensure_client_loaded(state) {
        Ok(client) => client,
        Err(err) => {
            eprintln!(
                "[TRACKING] attempt {current_attempt} for {video_id} could not load auth client: {err}"
            );
            return None;
        }
    };

    let queued_track = state
        .queue
        .get(state.queue_index)
        .filter(|track| track.video_id == video_id)
        .or_else(|| state.queue.iter().find(|track| track.video_id == video_id));
    let playback_playlist_id = queued_track
        .and_then(|track| track.playback_playlist_id.clone())
        .or_else(|| {
            state
                .playlist_context
                .as_ref()
                .map(|context| context.playlist_id.clone())
        });
    let playback_params = queued_track.and_then(|track| track.playback_params.clone());

    if let Some(tracking) = client.start_playback_tracking(
        video_id,
        session_started_at,
        playback_playlist_id.as_deref(),
        playback_params.as_deref(),
    ) {
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
    state.client_cookie_fingerprint = None;

    let refreshed_client = match ensure_client_loaded(state) {
        Ok(client) => client,
        Err(err) => {
            eprintln!(
                "[TRACKING] attempt {current_attempt} for {video_id} could not refresh auth client: {err}"
            );
            return None;
        }
    };

    let tracking = refreshed_client.start_playback_tracking(
        video_id,
        session_started_at,
        playback_playlist_id.as_deref(),
        playback_params.as_deref(),
    );
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

    std::thread::spawn(move || {
        let Some(cookie) = get_cookie() else {
            let _ = background_tx.send(BackgroundEvent::RadioExtensionReady {
                anchor_video_id,
                tracks: Vec::new(),
                chips: Vec::new(),
            });
            return;
        };

        let client = match NetworkState::from_cookie(&cookie) {
            Ok(client) => client,
            Err(err) => {
                eprintln!("[RADIO] failed to initialize network state: {err}");
                let _ = background_tx.send(BackgroundEvent::RadioExtensionReady {
                    anchor_video_id,
                    tracks: Vec::new(),
                    chips: Vec::new(),
                });
                return;
            }
        };
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
    if !track.reporting_enabled {
        return;
    }

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
    if track.awaiting_fresh_position {
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
    if is_logically_paused(state) {
        return;
    }

    let Some(track) = state.active_track.as_mut() else {
        return;
    };

    let player_position_millis = state
        .player
        .position()
        .map(|pos| pos.nseconds() / 1_000_000);
    if track.awaiting_fresh_position {
        match player_position_millis {
            Some(position_millis) if playback_position_confirms_start(track, position_millis) => {
                let session_elapsed_millis = track.started_at.elapsed().as_millis() as u64;
                track.awaiting_fresh_position = false;
                track.startup_seek_millis = None;
                eprintln!(
                    "[DAEMON] confirmed playback start for {} at {} ms after {} ms",
                    track.video_id, position_millis, session_elapsed_millis
                );
            }
            Some(position_millis) => {
                if track.last_position_millis == 0 || position_millis < track.last_position_millis {
                    track.last_position_millis = position_millis;
                }
                return;
            }
            None => return,
        }
    }

    let session_elapsed_millis = track.started_at.elapsed().as_millis() as u64;
    let listened = player_position_millis
        .filter(|millis| *millis > 0)
        .unwrap_or(session_elapsed_millis);
    track.last_position_millis = listened;

    if track.reporting_enabled {
        if let Some(tracking) = track.tracking.as_ref() {
            tracking.maybe_report_atr(listened);
            tracking.maybe_report_watchtime(listened);
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

fn note_sponsorblock_auto_skip(
    track: &mut ActiveTrack,
    segment: &SponsorSegment,
    from_millis: u64,
    target_millis: u64,
) {
    if track
        .sponsorblock_skipped_segment_ids
        .insert(segment.uuid.clone())
    {
        track.sponsorblock_auto_skip_offset_millis = track
            .sponsorblock_auto_skip_offset_millis
            .saturating_add(target_millis.saturating_sub(from_millis));
    }
}

fn maybe_apply_sponsorblock_skip(state: &mut DaemonState) {
    if is_logically_paused(state) {
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

    let from_millis = position.mseconds();
    let target_millis = (target_seconds * 1_000.0).round() as u64;
    eprintln!(
        "[SPONSORBLOCK] skipping {} segment on {} to {:.2}s",
        segment.category, video_id, target_seconds
    );
    if let Some(track) = state.active_track.as_mut() {
        note_sponsorblock_auto_skip(track, &segment, from_millis, target_millis);
    }
    let target = ClockTime::from_mseconds(target_millis);
    seek_to_position(state, target);

    if state
        .active_track
        .as_ref()
        .is_some_and(|track| track.reporting_enabled)
        && state.sponsor_segments_reported.insert(segment.uuid.clone())
    {
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
    let to_fetch = prefetch_candidate_indices(state.queue.len(), state.queue_index, prefetch_ahead)
        .into_iter()
        .filter_map(|index| state.queue.get(index))
        .filter(|track| {
            !state.cache.is_cached(&track.video_id)
                && state.cache.get_temp_audio(&track.video_id).is_none()
                && !state.prefetch_in_flight.contains(&track.video_id)
        })
        .map(|track| track.video_id.clone())
        .collect::<Vec<_>>();

    if to_fetch.is_empty() {
        return;
    }

    state.prefetch_in_flight.extend(to_fetch.iter().cloned());

    let cookie = get_cookie();
    let storage_root = get_storage_root();
    let background_tx = state.background_tx.clone();

    std::thread::spawn(move || {
        let Some(cookie) = cookie else {
            for video_id in to_fetch {
                let _ = background_tx.send(BackgroundEvent::PrefetchFinished { video_id });
            }
            return;
        };

        let cache = match MusicCache::new_with_storage_root(storage_root.as_deref()) {
            Ok(cache) => cache,
            Err(err) => {
                eprintln!("[PREFETCH] failed to initialize music cache: {err}");
                for video_id in to_fetch {
                    let _ = background_tx.send(BackgroundEvent::PrefetchFinished { video_id });
                }
                return;
            }
        };

        let client = match NetworkState::from_cookie(&cookie) {
            Ok(client) => client,
            Err(err) => {
                eprintln!("[PREFETCH] failed to initialize network state: {err}");
                for video_id in to_fetch {
                    let _ = background_tx.send(BackgroundEvent::PrefetchFinished { video_id });
                }
                return;
            }
        };

        for video_id in to_fetch {
            if cache.is_cached(&video_id) || cache.get_temp_audio(&video_id).is_some() {
                let _ = background_tx.send(BackgroundEvent::PrefetchFinished { video_id });
                continue;
            }

            let Ok(download) = download_audio_for_cache(&cache, &client, &video_id, "PREFETCH")
            else {
                let _ = background_tx.send(BackgroundEvent::PrefetchFinished { video_id });
                continue;
            };

            cache.store_temp_audio(&download.video_id, &download.audio);
            let _ = background_tx.send(BackgroundEvent::PrefetchFinished { video_id });
        }
    });
}

fn prefetch_candidate_indices(
    queue_len: usize,
    queue_index: usize,
    prefetch_ahead: usize,
) -> Vec<usize> {
    if queue_len == 0 {
        return Vec::new();
    }

    let mut indices = Vec::new();
    if queue_index > 0 {
        indices.push(queue_index - 1);
    }

    let start = queue_index.saturating_add(1);
    indices.extend(start..start.saturating_add(prefetch_ahead).min(queue_len));
    indices
}

fn history_thumbnail_candidate(url: Option<&str>) -> Option<&str> {
    url.filter(|value| !value.starts_with("file://"))
}

fn queued_track_snapshot(state: &DaemonState) -> Option<ActiveTrackSnapshot> {
    state
        .queue
        .get(state.queue_index)
        .map(|track| ActiveTrackSnapshot {
            video_id: track.video_id.clone(),
            title: track.title.clone(),
            artist: artist_string(track),
            thumb_url: track.thumbnail_url.clone(),
            rating: SongRating::Indifferent,
        })
}

fn persisted_playback_track_snapshot(state: &DaemonState) -> Option<ActiveTrackSnapshot> {
    if state.probe_context.is_some() {
        return queued_track_snapshot(state);
    }

    state
        .active_track
        .as_ref()
        .map(|track| ActiveTrackSnapshot {
            video_id: track.video_id.clone(),
            title: track.title.clone(),
            artist: track.artist.clone(),
            thumb_url: track.thumb_url.clone(),
            rating: track.rating,
        })
        .or_else(|| queued_track_snapshot(state))
}

fn persisted_playback_snapshot(state: &DaemonState) -> Option<PlaybackSnapshot> {
    if state.queue.is_empty() {
        return None;
    }

    let position_millis = state
        .probe_context
        .as_ref()
        .and_then(|probe| probe.position_millis)
        .or_else(|| pause_anchor_position_millis(state));
    let hide_timing = state.probe_context.is_none()
        && state
            .active_track
            .as_ref()
            .is_some_and(hide_timing_for_track);

    let mut snapshot = PlaybackSnapshot {
        current_track: persisted_playback_track_snapshot(state),
        queue: state.queue.clone(),
        queue_index: state.queue_index.min(state.queue.len().saturating_sub(1)),
        playlist_context: state.playlist_context.clone(),
        radio_virtual_playlist: state.radio_virtual_playlist.clone(),
        repeat_mode: state.repeat_mode,
        track_repeat_loops_left: state.track_repeat_loops_left,
        paused: state.probe_context.is_some()
            || (state.active_track.is_some() && is_logically_paused(state)),
        volume: logical_volume(state),
        position_millis,
        duration_millis: (state.probe_context.is_none() && !hide_timing)
            .then(|| visible_duration_millis(state))
            .flatten(),
        buffered_fraction: state
            .probe_context
            .is_none()
            .then_some(
                state
                    .active_track
                    .as_ref()
                    .map(|_| state.player.buffered_fraction())
                    .unwrap_or(0.0),
            )
            .unwrap_or(0.0),
        halt_auto_resume_pending: false,
        probe_active: false,
        probe_restore_position_millis: None,
        sleep_remaining_secs: None,
        sleep_after_current: false,
    };
    rearm_track_repeat_loops_for_snapshot(&mut snapshot, &state.settings);
    Some(snapshot)
}

fn state_snapshot(state: &DaemonState) -> PlaybackSnapshot {
    let probe_active = state.probe_context.is_some();
    let hide_timing = !probe_active
        && state
            .active_track
            .as_ref()
            .is_some_and(hide_timing_for_track);
    let sleep_status = sleep_status_snapshot(state);

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
        playlist_context: state.playlist_context.clone(),
        radio_virtual_playlist: state.radio_virtual_playlist.clone(),
        repeat_mode: state.repeat_mode,
        track_repeat_loops_left: state.track_repeat_loops_left,
        paused: is_logically_paused(state),
        volume: logical_volume(state),
        position_millis: (!hide_timing)
            .then(|| visible_position_millis(state))
            .flatten(),
        duration_millis: (!hide_timing)
            .then(|| visible_duration_millis(state))
            .flatten(),
        buffered_fraction: state.player.buffered_fraction(),
        halt_auto_resume_pending: mpris_halt_auto_resume_is_pending(state),
        probe_active,
        probe_restore_position_millis: state
            .probe_context
            .as_ref()
            .and_then(|probe| probe.position_millis),
        sleep_remaining_secs: sleep_status.remaining_secs,
        sleep_after_current: sleep_status.after_current,
    }
}

fn coordination_targets_snapshot(state: &mut DaemonState) -> CoordinationTargetsSnapshot {
    CoordinationTargetsSnapshot {
        mpris: state.peer_mpris.refresh_known_targets(),
        pipewire: pipewire_target_snapshots(state),
    }
}

fn pipewire_target_snapshots(state: &DaemonState) -> Vec<CoordinationTargetSnapshot> {
    let mut targets = state
        .pipewire_monitor
        .active_streams()
        .values()
        .map(|stream| {
            let key = stream.stable_filter_key().unwrap_or_default();
            CoordinationTargetSnapshot {
                filterable: !key.is_empty(),
                key,
                label: stream.label(),
                detail: stream.detail(),
                active: true,
            }
        })
        .collect::<Vec<_>>();

    // Surface configured filter entries that aren't currently live, so the user
    // can review and clear them (e.g. un-blacklist a stream) without the source
    // app running. Without this, the chooser only lists live streams and a
    // previously blacklisted entry can never be toggled back off from the UI.
    let mut present: std::collections::HashSet<String> = targets
        .iter()
        .flat_map(|target| {
            [
                normalize_coordination_identifier(&target.key),
                normalize_coordination_identifier(&target.label),
            ]
        })
        .filter(|identifier| !identifier.is_empty())
        .collect();
    for entry in &state.settings.pipewire_filter_list {
        let normalized = normalize_coordination_identifier(entry);
        if normalized.is_empty() || !present.insert(normalized) {
            continue;
        }
        targets.push(CoordinationTargetSnapshot {
            filterable: true,
            key: entry.clone(),
            label: entry.clone(),
            detail: String::new(),
            active: false,
        });
    }

    targets.sort_by(|left, right| {
        left.label
            .to_ascii_lowercase()
            .cmp(&right.label.to_ascii_lowercase())
            .then_with(|| left.key.cmp(&right.key))
    });
    targets.dedup_by(|left, right| {
        pipewire_target_dedupe_key(left) == pipewire_target_dedupe_key(right)
    });
    targets
}

fn pipewire_target_dedupe_key(target: &CoordinationTargetSnapshot) -> String {
    if !target.key.trim().is_empty() {
        format!("key:{}", target.key.to_ascii_lowercase())
    } else {
        format!(
            "unidentified:{}:{}",
            target.label.to_ascii_lowercase(),
            target.detail.to_ascii_lowercase()
        )
    }
}

fn maybe_persist_playback_state(state: &mut DaemonState, force: bool) {
    let Some(snapshot) = persisted_playback_snapshot(state) else {
        if force || state.last_persisted_playback_json.is_some() {
            if let Err(err) = clear_persisted_playback_state() {
                eprintln!("[STATICD] failed to clear persisted playback state: {err}");
            } else {
                state.last_persisted_playback_json = None;
                state.last_persisted_playback_at = Some(Instant::now());
            }
        }
        return;
    };

    if !force
        && state.active_track.is_some()
        && !is_logically_paused(state)
        && state
            .last_persisted_playback_at
            .is_some_and(|saved_at| saved_at.elapsed() < PLAYBACK_STATE_PERSIST_INTERVAL)
    {
        return;
    }

    let snapshot_json = match serde_json::to_string(&snapshot) {
        Ok(snapshot_json) => snapshot_json,
        Err(err) => {
            eprintln!("[STATICD] failed to serialize persisted playback state: {err}");
            return;
        }
    };

    if !force
        && state
            .last_persisted_playback_json
            .as_deref()
            .is_some_and(|saved| saved == snapshot_json)
    {
        return;
    }

    let payload = PersistedPlaybackState {
        snapshot,
        saved_at_unix_millis: current_unix_millis(),
    };
    let playlist_context = payload.snapshot.playlist_context.clone();

    match store_persisted_playback_state(&payload) {
        Ok(()) => {
            state.last_persisted_playback_json = Some(snapshot_json);
            state.last_persisted_playback_at = Some(Instant::now());
            if let Some(playlist_context) = playlist_context {
                if let Err(err) = store_persisted_playlist_state(&playlist_context, &payload) {
                    eprintln!(
                        "[STATICD] failed to persist playlist state for {}: {err}",
                        playlist_context.playlist_id
                    );
                }
            }
        }
        Err(err) => eprintln!("[STATICD] failed to persist playback state: {err}"),
    }
}

fn sync_mpris(state: &DaemonState) {
    if let Some(bridge) = state.mpris.borrow().as_ref() {
        bridge.sync_all(state);
        note_mpris_position_sync(state, mpris_position_millis(state), Instant::now());
    }
}

fn note_mpris_position_sync(state: &DaemonState, position_millis: Option<u64>, synced_at: Instant) {
    state.last_mpris_position_millis.set(position_millis);
    state.last_mpris_position_sync_at.set(Some(synced_at));
}

fn mpris_halt_auto_resume_is_pending(state: &DaemonState) -> bool {
    state.mpris_halt_auto_resume.is_pending()
}

fn mpris_halt_auto_resume_sidechannel_active(state: &DaemonState) -> bool {
    should_expose_mpris_halt_auto_sidechannel(
        mpris_halt_auto_resume_is_pending(state),
        state.settings.other_mpris_mode,
        state.settings.other_mpris_halt_auto_resume_secs,
        state.active_track.is_some(),
        is_logically_paused(state),
        state.peer_mpris.is_waiting_for_peer_pause(),
        !state.peer_mpris.playing_services().is_empty(),
    )
}

fn should_expose_mpris_halt_auto_sidechannel(
    pending: bool,
    mode: CoordinationMode,
    delay_secs: u32,
    has_active_track: bool,
    logically_paused: bool,
    waiting_for_peer: bool,
    peer_playing: bool,
) -> bool {
    pending
        && mode == CoordinationMode::HaltAuto
        && delay_secs > 0
        && has_active_track
        && logically_paused
        && waiting_for_peer
        && !peer_playing
}

fn clear_mpris_halt_auto_resume_pending(state: &mut DaemonState) -> bool {
    let was_pending = mpris_halt_auto_resume_is_pending(state);
    state.mpris_halt_auto_resume = MprisHaltAutoResumeState::Idle;
    was_pending
}

fn freeze_local_playback_for_mpris_halt_auto(state: &mut DaemonState) {
    let Some(position_millis) = pause_anchor_position_millis(state) else {
        return;
    };

    if let Some(track) = state.active_track.as_mut() {
        track.last_position_millis = position_millis;
    }

    state.volume_ramp = None;
    state.player.set_volume(0.0);
    state.player.pause();
    state.player.seek(ClockTime::from_mseconds(position_millis));
}

fn activate_mpris_halt_auto_resume_pending(state: &mut DaemonState) {
    if mpris_halt_auto_resume_is_pending(state) {
        return;
    }

    freeze_local_playback_for_mpris_halt_auto(state);
    state.mpris_halt_auto_resume = MprisHaltAutoResumeState::Pending;
    let waited_for = state.peer_mpris.waiting_for_services().join(", ");
    eprintln!(
        "[MPRIS] peer player(s) went quiet; holding Static in pending halt-auto resume: {waited_for}"
    );
    sync_mpris(state);
}

fn cancel_pending_mpris_halt_auto_resume(state: &mut DaemonState, reason: &str) -> bool {
    if !mpris_halt_auto_resume_is_pending(state) {
        return false;
    }

    freeze_local_playback_for_mpris_halt_auto(state);
    state.peer_mpris.clear_waiting();
    clear_mpris_halt_auto_resume_pending(state);
    eprintln!("[MPRIS] cancelled pending halt-auto resume from {reason}");
    sync_mpris(state);
    maybe_persist_playback_state(state, true);
    true
}

fn resume_pending_mpris_halt_auto(
    state: &mut DaemonState,
    transition: PlaybackTransition,
    reason: &str,
) -> bool {
    if !mpris_halt_auto_resume_is_pending(state) {
        return false;
    }

    state.peer_mpris.clear_waiting();
    clear_mpris_halt_auto_resume_pending(state);
    eprintln!("[MPRIS] resuming pending halt-auto playback from {reason}");
    resume_local_playback(state, transition);
    sync_mpris(state);
    maybe_persist_playback_state(state, true);
    true
}

fn handle_pending_mpris_halt_auto_transport_intent(
    state: &mut DaemonState,
    intent: PendingMprisTransportIntent,
) -> bool {
    match pending_mpris_transport_decision(mpris_halt_auto_resume_is_pending(state), intent) {
        PendingMprisTransportDecision::Ignore => false,
        PendingMprisTransportDecision::Cancel => {
            cancel_pending_mpris_halt_auto_resume(state, intent.label())
        }
        PendingMprisTransportDecision::ResumeWithoutApplying => {
            let command = intent.label();
            eprintln!(
                "[MPRIS] {command} received during pending halt-auto resume; resuming without applying it"
            );
            resume_pending_mpris_halt_auto(state, PlaybackTransition::Immediate, command)
        }
    }
}

fn pending_mpris_transport_decision(
    pending: bool,
    intent: PendingMprisTransportIntent,
) -> PendingMprisTransportDecision {
    if !pending {
        return PendingMprisTransportDecision::Ignore;
    }

    match intent {
        PendingMprisTransportIntent::Cancel(_) => PendingMprisTransportDecision::Cancel,
        PendingMprisTransportIntent::ResumeWithoutApplying(_) => {
            PendingMprisTransportDecision::ResumeWithoutApplying
        }
    }
}

fn return_pending_mpris_halt_auto_resume_to_peer_block(
    state: &mut DaemonState,
    playing_services: &[String],
) {
    state.peer_mpris.set_waiting_for_services(playing_services);
    if clear_mpris_halt_auto_resume_pending(state) {
        eprintln!(
            "[MPRIS] peer player(s) became active again; cancelling pending halt-auto resume: {}",
            playing_services.join(", ")
        );
        sync_mpris(state);
    }
}

fn mpris_halt_auto_peer_transition(
    mode: CoordinationMode,
    has_active_track: bool,
    logically_paused: bool,
    waiting_for_peer: bool,
    peer_playing: bool,
    pending: bool,
    delay_secs: u32,
    quiet_for: Option<Duration>,
) -> MprisHaltAutoPeerTransition {
    if !mode.coordinates_peers() {
        return MprisHaltAutoPeerTransition::DeactivateCoordination;
    }

    if !has_active_track || !logically_paused || !waiting_for_peer {
        return MprisHaltAutoPeerTransition::Ignore;
    }

    if peer_playing {
        return MprisHaltAutoPeerTransition::ReturnToPeerBlock;
    }

    if mode != CoordinationMode::HaltAuto {
        return if pending {
            MprisHaltAutoPeerTransition::ClearPending
        } else {
            MprisHaltAutoPeerTransition::Wait
        };
    }

    if delay_secs > 0 && !pending {
        return MprisHaltAutoPeerTransition::ActivatePending;
    }

    if should_resume_after_external_mpris_pause(
        mode,
        quiet_for,
        Duration::from_secs(delay_secs as u64),
    ) {
        MprisHaltAutoPeerTransition::Resume
    } else {
        MprisHaltAutoPeerTransition::Wait
    }
}

fn logical_volume(state: &DaemonState) -> f64 {
    state.pre_duck_volume
}

fn steady_output_volume(state: &DaemonState) -> f64 {
    if state.ducked {
        pipewire_duck_output_volume(state)
    } else {
        logical_volume(state)
    }
}

fn pipewire_duck_output_volume(state: &DaemonState) -> f64 {
    (state.pre_duck_volume
        * state
            .settings
            .pipewire_ducking_volume
            .clamp(0.0, PIPEWIRE_DUCKING_VOLUME_CLI_MAX))
    .clamp(0.0, PIPEWIRE_DUCKING_VOLUME_CLI_MAX)
}

fn set_logical_volume(state: &mut DaemonState, volume: f64) {
    let volume = volume.clamp(0.0, 1.0);
    state.pre_duck_volume = volume;

    if state.ducked {
        start_volume_ramp(state, pipewire_duck_output_volume(state));
    } else {
        state.volume_ramp = None;
        state.player.set_volume(volume);
    }
}

fn start_volume_ramp(state: &mut DaemonState, target_volume: f64) {
    start_volume_ramp_with_completion(
        state,
        target_volume,
        PIPEWIRE_VOLUME_RAMP_DURATION,
        VolumeRampCompletion::None,
    );
}

fn start_playback_volume_ramp(state: &mut DaemonState, target_volume: f64) {
    start_volume_ramp_with_completion(
        state,
        target_volume,
        PLAYBACK_VOLUME_RAMP_DURATION,
        VolumeRampCompletion::None,
    );
}

fn start_pause_fade(state: &mut DaemonState, position_millis: u64) {
    start_volume_ramp_with_completion(
        state,
        0.0,
        PLAYBACK_VOLUME_RAMP_DURATION,
        VolumeRampCompletion::PauseAtPosition { position_millis },
    );
}

fn start_volume_ramp_with_completion(
    state: &mut DaemonState,
    target_volume: f64,
    duration: Duration,
    completion: VolumeRampCompletion,
) {
    let target_volume = target_volume.clamp(0.0, PIPEWIRE_DUCKING_VOLUME_CLI_MAX);
    let current_volume = state.player.volume();
    state.player.set_volume(current_volume);
    if volumes_match(current_volume, target_volume) {
        finish_volume_ramp(state, target_volume, completion);
        return;
    }

    state.volume_ramp = Some(VolumeRamp {
        started_at: Instant::now(),
        start_volume: current_volume,
        target_volume,
        duration,
        completion,
    });
}

fn advance_volume_ramp(state: &mut DaemonState) {
    let Some(ramp) = state.volume_ramp else {
        return;
    };

    let next_volume = volume_ramp_value(ramp);
    state.player.set_volume(next_volume);

    if volume_ramp_is_complete(ramp, next_volume) {
        finish_volume_ramp(state, ramp.target_volume, ramp.completion);
    }
}

fn finish_volume_ramp(
    state: &mut DaemonState,
    final_volume: f64,
    completion: VolumeRampCompletion,
) {
    state
        .player
        .set_volume(final_volume.clamp(0.0, PIPEWIRE_DUCKING_VOLUME_CLI_MAX));
    state.volume_ramp = None;

    match completion {
        VolumeRampCompletion::None => {}
        VolumeRampCompletion::PauseAtPosition { position_millis } => {
            state.player.pause();
            state.player.seek(ClockTime::from_mseconds(position_millis));
            if let Some(track) = state.active_track.as_mut() {
                track.last_position_millis = position_millis;
            }
            sync_mpris(state);
            maybe_persist_playback_state(state, true);
            maybe_request_headless_sleep_shutdown(state, Instant::now());
        }
    }
}

fn volume_ramp_value(ramp: VolumeRamp) -> f64 {
    let progress =
        (ramp.started_at.elapsed().as_secs_f64() / ramp.duration.as_secs_f64()).clamp(0.0, 1.0);
    (ramp.start_volume + (ramp.target_volume - ramp.start_volume) * progress)
        .clamp(0.0, PIPEWIRE_DUCKING_VOLUME_CLI_MAX)
}

fn volumes_match(left: f64, right: f64) -> bool {
    (left - right).abs() <= 0.0001
}

fn volume_ramp_is_complete(ramp: VolumeRamp, next_volume: f64) -> bool {
    ramp.started_at.elapsed() >= ramp.duration || volumes_match(next_volume, ramp.target_volume)
}

fn desired_pipewire_suppression_mode(state: &DaemonState) -> PipeWireSuppressionMode {
    resolve_pipewire_suppression_mode(
        state.settings.pipewire_ducking_enabled,
        state.pipewire_trigger_active,
        state.active_track.is_some(),
        state.settings.pipewire_pause_mode,
        state.pipewire_pause_active,
        is_logically_paused(state),
        state.peer_mpris.is_waiting_for_peer_pause(),
    )
}

fn resolve_pipewire_suppression_mode(
    pipewire_ducking_enabled: bool,
    pipewire_trigger_active: bool,
    has_active_track: bool,
    pipewire_pause_mode: CoordinationMode,
    pipewire_pause_active: bool,
    logically_paused: bool,
    peer_mpris_pause_active: bool,
) -> PipeWireSuppressionMode {
    if !pipewire_ducking_enabled || !pipewire_trigger_active || !has_active_track {
        return PipeWireSuppressionMode::None;
    }

    if pipewire_pause_mode.is_enabled() {
        if pipewire_pause_active || !logically_paused || peer_mpris_pause_active {
            PipeWireSuppressionMode::Pause
        } else {
            PipeWireSuppressionMode::None
        }
    } else if peer_mpris_pause_active || logically_paused {
        PipeWireSuppressionMode::None
    } else {
        PipeWireSuppressionMode::Duck
    }
}

fn pipewire_pause_is_requested(state: &DaemonState) -> bool {
    state.settings.pipewire_ducking_enabled
        && state.pipewire_trigger_active
        && state.active_track.is_some()
        && state.settings.pipewire_pause_mode.is_enabled()
}

fn pipewire_unduck_can_replace_volume_ramp(volume_ramp: Option<VolumeRamp>) -> bool {
    !volume_ramp.is_some_and(|ramp| {
        matches!(
            ramp.completion,
            VolumeRampCompletion::PauseAtPosition { .. }
        )
    })
}

fn poll_pipewire_streams(state: &mut DaemonState) {
    if state
        .last_pipewire_poll_at
        .is_some_and(|last_poll_at| last_poll_at.elapsed() < PIPEWIRE_STREAM_POLL_INTERVAL)
    {
        return;
    }

    state.last_pipewire_poll_at = Some(Instant::now());
    let threshold =
        Duration::from_secs(state.settings.pipewire_ducking_threshold_secs.max(1).into());
    let active_streams = state.pipewire_monitor.active_streams();

    state.pipewire_trigger_active = active_streams
        .values()
        .filter(|stream| {
            coordination_filter_allows(
                state.settings.pipewire_filter_mode,
                &state.settings.pipewire_filter_list,
                &stream.identifiers(),
            )
        })
        .any(|stream| stream.first_seen.elapsed() >= threshold);
    if state.pipewire_trigger_active {
        state.pipewire_quiet_since = None;
    } else {
        state.pipewire_quiet_since.get_or_insert_with(Instant::now);
    }
}

fn maybe_unduck_for_pipewire(state: &mut DaemonState) {
    let desired = desired_pipewire_suppression_mode(state);

    if state.ducked && desired != PipeWireSuppressionMode::Duck {
        state.ducked = false;
        if pipewire_unduck_can_replace_volume_ramp(state.volume_ramp) {
            start_volume_ramp(state, state.pre_duck_volume);
        }
    }

    if state.pipewire_pause_active && desired != PipeWireSuppressionMode::Pause {
        let peer_mpris_pause_active = state.peer_mpris.is_waiting_for_peer_pause();
        let should_resume = state.active_track.is_some()
            && is_logically_paused(state)
            && !peer_mpris_pause_active
            && should_resume_after_pipewire_pause(
                state.settings.pipewire_pause_mode,
                desired,
                state
                    .pipewire_quiet_since
                    .map(|quiet_since| quiet_since.elapsed()),
                Duration::from_secs(state.settings.pipewire_pause_halt_auto_resume_secs as u64),
            );
        let should_clear_without_resuming = desired == PipeWireSuppressionMode::None
            && !peer_mpris_pause_active
            && matches!(state.settings.pipewire_pause_mode, CoordinationMode::Halt);

        if should_resume {
            state.pipewire_pause_active = false;
            resume_local_playback(state, PlaybackTransition::Calm);
            sync_mpris(state);
        } else if should_clear_without_resuming {
            state.pipewire_pause_active = false;
        }
    }
}

fn maybe_duck_for_pipewire(state: &mut DaemonState) {
    match desired_pipewire_suppression_mode(state) {
        PipeWireSuppressionMode::Duck => {
            let target_volume = pipewire_duck_output_volume(state);
            if !state.ducked {
                state.pre_duck_volume = logical_volume(state);
                state.ducked = true;
                start_volume_ramp(state, target_volume);
            } else if state.volume_ramp.map_or(true, |ramp| {
                !volumes_match(ramp.target_volume, target_volume)
            }) && !volumes_match(state.player.volume(), target_volume)
            {
                start_volume_ramp(state, target_volume);
            }
        }
        PipeWireSuppressionMode::Pause => {
            if !state.pipewire_pause_active {
                state.pre_duck_volume = logical_volume(state);
                state.pipewire_pause_active = true;
            }
            if !is_logically_paused(state) {
                pause_local_playback(state, PlaybackTransition::Calm);
                sync_mpris(state);
            }
        }
        PipeWireSuppressionMode::None => {}
    }
}

fn maybe_pause_for_external_mpris(state: &mut DaemonState) {
    if !state.settings.other_mpris_mode.coordinates_peers() {
        state.peer_mpris.clear_waiting();
        return;
    }

    if state.active_track.is_none() {
        return;
    }

    let playing_services = state.peer_mpris.playing_services().to_vec();
    if playing_services.is_empty() {
        return;
    }

    if is_logically_paused(state) {
        if state.pipewire_pause_active || state.peer_mpris.is_waiting_for_peer_pause() {
            state.peer_mpris.set_waiting_for_services(&playing_services);
        }
        return;
    }

    eprintln!(
        "[MPRIS] pausing local playback because another player is active: {}",
        playing_services.join(", ")
    );
    state.peer_mpris.set_waiting_for_services(&playing_services);
    pause_local_playback(state, PlaybackTransition::Calm);
    sync_mpris(state);
}

fn pause_other_mpris_players(state: &mut DaemonState) {
    state.peer_mpris.clear_waiting();
    if !state.settings.other_mpris_mode.coordinates_peers() {
        return;
    }

    let settings = state.settings.clone();
    let paused_services = state.peer_mpris.pause_other_playing_services(&settings);
    if paused_services.is_empty() {
        return;
    }

    eprintln!(
        "[MPRIS] paused peer player(s) before starting local playback: {}",
        paused_services.join(", ")
    );
}

fn maybe_resume_after_external_mpris(state: &mut DaemonState) {
    let other_mpris_mode = state.settings.other_mpris_mode;
    let playing_services = state.peer_mpris.playing_services().to_vec();
    let transition = mpris_halt_auto_peer_transition(
        other_mpris_mode,
        state.active_track.is_some(),
        is_logically_paused(state),
        state.peer_mpris.is_waiting_for_peer_pause(),
        !playing_services.is_empty(),
        mpris_halt_auto_resume_is_pending(state),
        state.settings.other_mpris_halt_auto_resume_secs,
        state.peer_mpris.no_playing_services_for(),
    );

    match transition {
        MprisHaltAutoPeerTransition::DeactivateCoordination => {
            if clear_mpris_halt_auto_resume_pending(state) {
                sync_mpris(state);
            }
            state.peer_mpris.clear_waiting();
            return;
        }
        MprisHaltAutoPeerTransition::Ignore | MprisHaltAutoPeerTransition::Wait => return,
        MprisHaltAutoPeerTransition::ReturnToPeerBlock => {
            return_pending_mpris_halt_auto_resume_to_peer_block(state, &playing_services);
            return;
        }
        MprisHaltAutoPeerTransition::ClearPending => {
            clear_mpris_halt_auto_resume_pending(state);
            sync_mpris(state);
            return;
        }
        MprisHaltAutoPeerTransition::ActivatePending => {
            activate_mpris_halt_auto_resume_pending(state);
            return;
        }
        MprisHaltAutoPeerTransition::Resume => {
            clear_mpris_halt_auto_resume_pending(state);
        }
    }

    let waited_for = state.peer_mpris.waiting_for_services().join(", ");
    state.peer_mpris.clear_waiting();
    if pipewire_pause_is_requested(state) {
        state.pipewire_pause_active = true;
        sync_mpris(state);
        return;
    }

    match other_mpris_mode {
        CoordinationMode::HaltAuto => eprintln!(
            "[MPRIS] resuming local playback after peer player(s) stayed paused or stopped for {}s: {waited_for}",
            state.settings.other_mpris_halt_auto_resume_secs
        ),
        CoordinationMode::Off | CoordinationMode::Halt => return,
    }
    resume_local_playback(state, PlaybackTransition::Calm);
    sync_mpris(state);
}

fn poll_external_mpris(state: &mut DaemonState) {
    if !state.settings.other_mpris_mode.coordinates_peers() {
        state.peer_mpris.deactivate();
        return;
    }

    if state.active_track.is_none() {
        return;
    }

    let settings = state.settings.clone();
    state.peer_mpris.poll_if_due(&settings);
}

fn should_resume_after_external_mpris_pause(
    mode: CoordinationMode,
    quiet_for: Option<Duration>,
    halt_auto_resume_after: Duration,
) -> bool {
    match mode {
        CoordinationMode::HaltAuto => {
            quiet_for.is_some_and(|quiet_for| quiet_for >= halt_auto_resume_after)
        }
        CoordinationMode::Off | CoordinationMode::Halt => false,
    }
}

fn should_resume_after_pipewire_pause(
    mode: CoordinationMode,
    desired: PipeWireSuppressionMode,
    quiet_for: Option<Duration>,
    halt_auto_resume_after: Duration,
) -> bool {
    match desired {
        PipeWireSuppressionMode::Duck => true,
        PipeWireSuppressionMode::Pause => false,
        PipeWireSuppressionMode::None => match mode {
            CoordinationMode::Off => true,
            CoordinationMode::Halt => false,
            CoordinationMode::HaltAuto => {
                quiet_for.is_some_and(|quiet_for| quiet_for >= halt_auto_resume_after)
            }
        },
    }
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

fn inspect_mpris_peer(bus_name: &str) -> Result<MprisPeerInfo> {
    let player = peer_player_for_bus(bus_name)?;
    let app_key = mpris_app_key_from_bus(bus_name);
    let identity = non_empty_string(player.identity().to_string());
    let desktop_entry = player
        .get_desktop_entry()
        .ok()
        .flatten()
        .and_then(non_empty_string);
    let playing = matches!(
        player.get_playback_status(),
        Ok(PeerPlaybackStatus::Playing)
    );

    Ok(MprisPeerInfo {
        bus_name: bus_name.to_string(),
        app_key,
        identity,
        desktop_entry,
        playing,
    })
}

fn mpris_app_key_from_bus(bus_name: &str) -> Option<String> {
    let app = bus_name.strip_prefix("org.mpris.MediaPlayer2.")?;
    let app = app
        .split_once(".instance")
        .map(|(app, _)| app)
        .unwrap_or(app);
    non_empty_string(app.to_string())
}

fn mpris_target_snapshots(peers: &[MprisPeerInfo]) -> Vec<CoordinationTargetSnapshot> {
    let mut targets = peers
        .iter()
        .map(MprisPeerInfo::target_snapshot)
        .collect::<Vec<_>>();
    targets.sort_by(|left, right| {
        left.label
            .to_ascii_lowercase()
            .cmp(&right.label.to_ascii_lowercase())
            .then_with(|| left.key.cmp(&right.key))
    });
    targets.dedup_by(|left, right| left.key.eq_ignore_ascii_case(&right.key));
    targets
}

fn non_empty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
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

    if let Some(duration_millis) = mpris_duration_millis(state) {
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
        "com.static.music",
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
    crate::network::get_cookie()
}

fn get_vr_refresh_token() -> Option<String> {
    crate::network::get_vr_refresh_token()
}

fn get_storage_root() -> Option<String> {
    crate::network::get_storage_root()
}

#[cfg(test)]
#[path = "tests/daemon.rs"]
mod tests;
