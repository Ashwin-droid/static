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

use crate::desktop_volume::{self, DesktopAwareVolume, DesktopVolumeInputs, SinkSample};
use crate::ipc::{
    ActiveTrackSnapshot, CoordinationTargetSnapshot, CoordinationTargetsSnapshot, DAEMON_BUS_NAME,
    DAEMON_OBJECT_PATH, DaemonRequest, DaemonResponse, PersistedPlaybackState, PlaybackSnapshot,
    PlaylistContext, RepeatMode, SleepStatusSnapshot, clear_persisted_playback_state,
    current_unix_millis, store_persisted_playback_state, store_persisted_playlist_state,
    youtube_music_watch_url,
};
use crate::models::{Artist, RadioChip, RadioVirtualPlaylist, SyncedLyrics, Track};
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
const DESYNC_PLAYBACK_START_DELAY: Duration = Duration::from_secs(1);
const MPRIS_POSITION_SYNC_INTERVAL: Duration = Duration::from_millis(250);
const EXTERNAL_MPRIS_POLL_INTERVAL: Duration = Duration::from_millis(250);
const EXTERNAL_MPRIS_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(30);
const MPRIS_PENDING_POSITION_SYNC_INTERVAL: Duration = Duration::from_secs(1);
const PLAYBACK_VOLUME_RAMP_DURATION: Duration = Duration::from_secs(2);
const PIPEWIRE_STREAM_POLL_INTERVAL: Duration = Duration::from_millis(500);
const PIPEWIRE_VOLUME_RAMP_DURATION: Duration = Duration::from_millis(250);
const DESKTOP_VOLUME_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// Connectivity cache freshness for the offline radio-shuffle advance fallback.
/// Desktop has no synchronous connectivity oracle (Android parity: the port asks
/// ConnectivityManager at advance time), so a background TCP probe against the
/// streaming endpoint refreshes a cached verdict every few seconds while a
/// shuffled radio queue is playing; the advance path only ever reads the cache
/// and never blocks on the network.
const CONNECTIVITY_PROBE_INTERVAL: Duration = Duration::from_secs(5);
const CONNECTIVITY_PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
const UI_CLIENT_LEASE_TTL: Duration = Duration::from_secs(20);
const DBUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
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

struct PendingPlaybackStart {
    due_at: Instant,
    start_when_due: bool,
    queue_index: usize,
    video_id: String,
    startup_retries: u8,
    budget_mode: TrackRepeatBudgetMode,
    suppress_reporting: bool,
    preserved_track_repeat_loops_left: Option<u32>,
    /// Staged by a forward advance (user Next), not an explicit song choice:
    /// only these starts may be covered by seamless listening when they fail.
    forward_advance: bool,
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
    cartridges: crate::cartridges::CartridgeStore,
    player: PlaybackEngine,
    cache: MusicCache,
    settings: AppSettings,
    client: Option<Arc<NetworkState>>,
    client_cookie_fingerprint: Option<u64>,
    queue: Vec<Track>,
    queue_index: usize,
    playlist_context: Option<PlaylistContext>,
    radio_virtual_playlist: Option<RadioVirtualPlaylist>,
    repeat_mode: RepeatMode,
    track_repeat_loops_left: Option<u32>,
    shuffle: bool,
    /// Pre-shuffle queue order; set on shuffle-on for playlist queues, cleared on
    /// shuffle-off and any genuine queue replacement. Daemon-session state only —
    /// never persisted, so after a restart disabling just keeps the current order.
    unshuffled_order: Option<Vec<Track>>,
    /// Pre-decided radio-shuffle pick (video id) for the NEXT forward advance,
    /// decided when a track starts so the prefetch worker downloads it first and
    /// the snapshot can advertise it as "Next up". Consumed (and cleared) by
    /// [`promote_random_untouched_pick`]; re-decided/validated by
    /// [`refresh_shuffle_up_next`] from [`prefetch_upcoming`], which every queue
    /// mutation and track start already funnels through. Daemon-session state
    /// only — never persisted (Android parity: PlaybackService.shuffleUpNextId).
    shuffle_up_next: Option<String>,
    /// Last connectivity probe verdict (see [`maybe_probe_connectivity`]),
    /// optimistically `true` until a probe fails so the offline advance
    /// restriction only engages on positive evidence of being offline.
    online: bool,
    last_connectivity_probe_at: Option<Instant>,
    connectivity_probe_in_flight: bool,
    active_track: Option<ActiveTrack>,
    pending_playback_start: Option<PendingPlaybackStart>,
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
    pipewire_trigger_since: Option<Instant>,
    pipewire_quiet_since: Option<Instant>,
    last_pipewire_poll_at: Option<Instant>,
    desktop_volume: DesktopAwareVolume,
    desktop_volume_io_in_flight: bool,
    last_desktop_volume_sample_at: Option<Instant>,
    ducked: bool,
    pipewire_pause_active: bool,
    pre_duck_volume: f64,
    volume_ramp: Option<VolumeRamp>,
    sleep_deadline: Option<Instant>,
    sleep_after_current: bool,
    sleep_shutdown_pending: bool,
    lyrics_cache: Option<(String, Option<SyncedLyrics>)>,
    ui_clients: HashMap<String, Instant>,
    last_persisted_playback_json: Option<String>,
    last_persisted_playback_at: Option<Instant>,
    shutting_down: Cell<bool>,
    /// In-daemon staticfs mount host (the daemon owns the FUSE driver).
    #[cfg(feature = "staticfs")]
    staticfs_host: crate::staticfs::host::HostHandle,
}

struct ExternalMprisCoordinator {
    last_poll_at: Option<Instant>,
    last_error_log_at: Option<Instant>,
    known_targets: Vec<CoordinationTargetSnapshot>,
    playing_services: Vec<String>,
    last_logged_playing_services: Vec<String>,
    waiting_for_services: Vec<String>,
    no_playing_services_since: Option<Instant>,
    /// When the current run of playing peers started (desktop aware volume
    /// uses it to pick the sink baseline from before the interruption).
    playing_services_since: Option<Instant>,
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
            playing_services_since: None,
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
            self.playing_services_since = None;
        } else {
            self.no_playing_services_since = None;
            self.playing_services_since.get_or_insert_with(Instant::now);
        }
    }

    fn has_playing_services(&self) -> bool {
        !self.playing_services.is_empty()
    }

    fn playing_services_since(&self) -> Option<Instant> {
        self.playing_services_since
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
        self.playing_services_since = None;
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
        if state.repeat_mode == RepeatMode::One && has_current_track(state) {
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
    /// Set by the D-Bus handler when it stops waiting for the reply
    /// (timeout/disconnect). The main loop skips abandoned envelopes it has
    /// not started yet: the caller was already told the request failed, so
    /// executing it late would run a command twice on retry (e.g. a
    /// ResolveAndQueue write queueing the same track again).
    abandoned: Arc<AtomicBool>,
}

enum BackgroundEvent {
    TrackingReady {
        video_id: String,
        attempt: u8,
        tracking: Option<TrackingInfo>,
    },
    PrefetchFinished {
        video_id: String,
    },
    ConnectivityProbeFinished {
        online: bool,
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
    SinkSampleReady {
        sample: Option<SinkSample>,
    },
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
        let abandoned = Arc::new(AtomicBool::new(false));
        self.command_tx
            .send(CommandEnvelope {
                request,
                reply: reply_tx,
                abandoned: abandoned.clone(),
            })
            .map_err(|_| zbus::fdo::Error::Failed("Daemon main loop is unavailable".into()))?;
        // Timeout/disconnect answer with `Unavailable`, NOT `Error`: the
        // request was never validated or rejected by the daemon, so clients
        // must treat this as a transport failure (staticfs maps it to EIO,
        // while `Error` means EINVAL). Marking the envelope abandoned keeps
        // the main loop from executing it late once it unblocks.
        let response = match reply_rx.recv_timeout(DBUS_REQUEST_TIMEOUT) {
            Ok(response) => response,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                abandoned.store(true, Ordering::SeqCst);
                DaemonResponse::Unavailable(format!(
                    "Daemon did not reply within {} seconds",
                    DBUS_REQUEST_TIMEOUT.as_secs()
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                abandoned.store(true, Ordering::SeqCst);
                DaemonResponse::Unavailable("Daemon did not reply".to_string())
            }
        };
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
                if has_current_track(&state) && is_logically_paused(&state) {
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
                if has_current_track(&state) && !is_logically_paused(&state) {
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
                let _ = advance_queue_for_user_action(&mut state, true);
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
                let _ = handle_previous_for_user_action(&mut state);
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
            player.connect_set_shuffle(move |_, enabled| {
                set_shuffle(&mut state.borrow_mut(), enabled);
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
        let shuffle = snapshot.shuffle;
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
            let _ = bridge.player.set_shuffle(shuffle).await;
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
    player.configure_gain(&settings);
    let initial_volume = player.volume();
    let (command_tx, command_rx) = mpsc::channel::<CommandEnvelope>();
    let (background_tx, background_rx) = mpsc::channel::<BackgroundEvent>();
    let state = Rc::new(RefCell::new(DaemonState {
        cartridges: crate::cartridges::CartridgeStore::default(),
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
        shuffle: false,
        unshuffled_order: None,
        shuffle_up_next: None,
        online: true,
        last_connectivity_probe_at: None,
        connectivity_probe_in_flight: false,
        active_track: None,
        pending_playback_start: None,
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
        pipewire_trigger_since: None,
        pipewire_quiet_since: Some(Instant::now()),
        last_pipewire_poll_at: None,
        desktop_volume: DesktopAwareVolume::default(),
        desktop_volume_io_in_flight: false,
        last_desktop_volume_sample_at: None,
        ducked: false,
        pipewire_pause_active: false,
        pre_duck_volume: initial_volume,
        volume_ramp: None,
        sleep_deadline: None,
        sleep_after_current: false,
        sleep_shutdown_pending: false,
        lyrics_cache: None,
        ui_clients: HashMap::new(),
        last_persisted_playback_json: None,
        last_persisted_playback_at: None,
        shutting_down: Cell::new(false),
        #[cfg(feature = "staticfs")]
        staticfs_host: crate::staticfs::host::HostHandle::spawn(),
    }));

    {
        let mut state = state.borrow_mut();
        schedule_offline_cache_sync(&mut state);
        sync_staticfs_host(&state);
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
            // The D-Bus handler already answered Unavailable for this
            // envelope; running it now would execute a command whose caller
            // was told it never completed.
            if envelope.abandoned.load(Ordering::SeqCst) {
                continue;
            }
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
    // Daemon death takes staticfs with it: unmount (clean, else lazy).
    #[cfg(feature = "staticfs")]
    state.staticfs_host.shutdown();
    state.shutting_down.set(true);
}

/// Pushes the current staticfs settings at the mount host (hot reload:
/// toggling `staticfs_enabled` mounts/unmounts the live filesystem, changing
/// `staticfs_mountpoint` remounts). No-op without the staticfs feature.
fn sync_staticfs_host(state: &DaemonState) {
    #[cfg(feature = "staticfs")]
    state.staticfs_host.set_desired(
        state.settings.staticfs_enabled,
        Some(state.settings.staticfs_mountpoint.clone()),
    );
    #[cfg(not(feature = "staticfs"))]
    let _ = state;
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
    state.unshuffled_order = None;

    if play {
        pause_other_mpris_players(state);
        return play_queue_at_for_user_action(
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

fn visualization_staticfs_mountpoint(settings: &AppSettings) -> Option<std::path::PathBuf> {
    #[cfg(feature = "staticfs")]
    {
        settings
            .staticfs_enabled
            .then(|| crate::staticfs::host::resolve_mountpoint(&settings.staticfs_mountpoint))
    }
    #[cfg(not(feature = "staticfs"))]
    {
        let _ = settings;
        None
    }
}

fn handle_cartridge_request(
    store: &mut crate::cartridges::CartridgeStore,
    staticfs_enabled: bool,
    request: DaemonRequest,
) -> DaemonResponse {
    use crate::cartridges::{CHUNK_BASE64_MAX, CHUNK_BYTES};
    use base64::{Engine, engine::general_purpose::STANDARD};
    match request {
        DaemonRequest::BeginCartridgeUpload { name } => {
            if !staticfs_enabled {
                return DaemonResponse::Error(
                    "Staticfs is disabled. Retry cartridge export to save to the filesystem."
                        .into(),
                );
            }
            match store.begin(name) {
                Ok(id) => DaemonResponse::CartridgeUpload(id),
                Err(error) => DaemonResponse::Error(error.to_string()),
            }
        }
        DaemonRequest::AppendCartridgeUpload { id, data_base64 } => {
            if data_base64.len() > CHUNK_BASE64_MAX {
                store.abort(&id);
                return DaemonResponse::Error("Cartridge chunk is too large".into());
            }
            match STANDARD.decode(data_base64) {
                Ok(bytes) if bytes.len() <= CHUNK_BYTES => match store.append(&id, bytes) {
                    Ok(()) => DaemonResponse::Ok,
                    Err(error) => DaemonResponse::Error(error.to_string()),
                },
                _ => {
                    store.abort(&id);
                    DaemonResponse::Error("Invalid cartridge chunk".into())
                }
            }
        }
        DaemonRequest::CommitCartridgeUpload { id } => match store.commit(&id) {
            Ok(info) => DaemonResponse::CartridgeCommitted(info),
            Err(error) => DaemonResponse::Error(error.to_string()),
        },
        DaemonRequest::AbortCartridgeUpload { id } => {
            store.abort(&id);
            DaemonResponse::Ok
        }
        DaemonRequest::ListMemoryCartridges => DaemonResponse::MemoryCartridges(store.list()),
        DaemonRequest::MemoryCartridgeInfo { name } => {
            DaemonResponse::MemoryCartridgeInfo(store.info(&name))
        }
        DaemonRequest::ReadMemoryCartridge { name, offset, size } => {
            DaemonResponse::CartridgeChunk(
                store
                    .read(&name, offset, size)
                    .map(|bytes| STANDARD.encode(bytes)),
            )
        }
        DaemonRequest::RemoveMemoryCartridge { name } => {
            DaemonResponse::CartridgeRemoved(store.remove(&name))
        }
        _ => DaemonResponse::Error("Unsupported cartridge request".into()),
    }
}

fn handle_request(state: &mut DaemonState, request: DaemonRequest) -> DaemonResponse {
    match request {
        request @ (DaemonRequest::BeginCartridgeUpload { .. }
        | DaemonRequest::AppendCartridgeUpload { .. }
        | DaemonRequest::CommitCartridgeUpload { .. }
        | DaemonRequest::AbortCartridgeUpload { .. }
        | DaemonRequest::ListMemoryCartridges
        | DaemonRequest::MemoryCartridgeInfo { .. }
        | DaemonRequest::ReadMemoryCartridge { .. }
        | DaemonRequest::RemoveMemoryCartridge { .. }) => handle_cartridge_request(
            &mut state.cartridges,
            state.settings.staticfs_enabled,
            request,
        ),
        DaemonRequest::Ping => DaemonResponse::Pong,
        DaemonRequest::GetState => DaemonResponse::State(state_snapshot(state)),
        DaemonRequest::GetGainDiagnostics => {
            let mut diagnostics = state.player.gain_diagnostics();
            diagnostics.playback_paused = is_logically_paused(state);
            diagnostics.track = current_track_snapshot(state);
            DaemonResponse::GainDiagnostics(diagnostics)
        }
        DaemonRequest::SetGainCalibration {
            settings: calibration,
        } => {
            let mut settings = match state.cache.load_settings() {
                Ok(settings) => settings,
                Err(error) => {
                    return DaemonResponse::Error(format!("Failed to load settings: {error}"));
                }
            };
            if let Err(error) = calibration.apply_to(&mut settings) {
                return DaemonResponse::Error(error.to_string());
            }
            if let Err(error) = state.cache.save_settings(&settings) {
                return DaemonResponse::Error(format!("Failed to save settings: {error}"));
            }
            reload_settings(state);
            sync_mpris(state);
            maybe_persist_playback_state(state, true);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::GetVisualizationFrame { bands } => {
            if !(8..=128).contains(&bands) {
                return DaemonResponse::Error(
                    "Visualization bands must be between 8 and 128".into(),
                );
            }
            DaemonResponse::VisualizationFrame(state.player.visualization_frame(bands as usize))
        }
        DaemonRequest::GetVisualizationSource {
            video_id,
            supports_memory,
        } => {
            if state.settings.staticfs_enabled && !supports_memory {
                return DaemonResponse::Error(
                    "Reopen the visualiser to use in-memory cartridges.".into(),
                );
            }
            match state.active_track.as_ref() {
                Some(track) if track.video_id == video_id => {
                    DaemonResponse::VisualizationSource(crate::ipc::VisualizationSource {
                        video_id: track.video_id.clone(),
                        title: track.title.clone(),
                        artist: track.artist.clone(),
                        // Link cartridges do not need an audio stream or local file.
                        uri: state.player.visualization_uri().unwrap_or_default(),
                        storage_root: state.cache.base_dir().to_path_buf(),
                        staticfs_enabled: state.settings.staticfs_enabled,
                        staticfs_mountpoint: visualization_staticfs_mountpoint(&state.settings),
                    })
                }
                _ => DaemonResponse::Error(
                    "The playing song changed. Capture the cartridge again.".into(),
                ),
            }
        }
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
            match play_queue_at_for_user_action(
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
            state.unshuffled_order = None;
            state.radio_append_in_flight = false;
            state.last_radio_append_anchor = None;
            state.pending_playback_start = None;
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
            if !valid_requested_volume(volume, &state.settings) {
                return DaemonResponse::Error(format!(
                    "volume must be finite and between 0 and {}",
                    state.settings.max_volume()
                ));
            }
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
        DaemonRequest::SetShuffle { enabled } => {
            set_shuffle(state, enabled);
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
        DaemonRequest::GetSettings => {
            let mut pairs = Vec::with_capacity(crate::cli::CONFIG_SPECS.len());
            for spec in crate::cli::CONFIG_SPECS {
                match crate::cli::format_config_value(&state.settings, spec.name) {
                    Ok(value) => pairs.push((spec.name.to_string(), value)),
                    Err(err) => return DaemonResponse::Error(err.to_string()),
                }
            }
            DaemonResponse::Settings(pairs)
        }
        DaemonRequest::SetSetting { key, value } => {
            // Load fresh settings from the database (not state.settings) so a
            // concurrent CLI write between daemon reloads is not clobbered.
            let mut settings = match state.cache.load_settings() {
                Ok(settings) => settings,
                Err(err) => {
                    return DaemonResponse::Error(format!("Failed to load settings: {err}"));
                }
            };
            if let Err(err) = crate::cli::apply_config_value(&mut settings, &key, &value) {
                return DaemonResponse::Error(err.to_string());
            }
            if let Err(err) = state.cache.save_settings(&settings) {
                return DaemonResponse::Error(format!("Failed to save settings: {err}"));
            }
            reload_settings(state);
            sync_mpris(state);
            maybe_persist_playback_state(state, true);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::ResolveAndQueue { query, next } => {
            let track = match ensure_client_loaded(state)
                .and_then(|client| crate::cli::resolve_track_input(&client, &query, true))
            {
                Ok(track) => track,
                Err(err) => return DaemonResponse::Error(err.to_string()),
            };
            // Same insert slots as the CLI (apply_queue_insert): `next` is
            // the `queue addNext` slot, otherwise the `queue addLast` one.
            let insert_at = if next {
                if state.queue.is_empty() {
                    0
                } else {
                    state.queue_index.saturating_add(1).min(state.queue.len())
                }
            } else {
                state
                    .radio_virtual_playlist
                    .as_ref()
                    .map(|radio| radio.touched_len.min(state.queue.len()))
                    .unwrap_or(state.queue.len())
            };
            insert_into_queue(state, vec![track], insert_at);
            maybe_persist_playback_state(state, true);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::ResolveAndPlayRadio {
            query,
            force_search,
        } => {
            // force_search states explicit "this is a song name" intent:
            // skip the bare-video-id interpretation entirely.
            let track = match ensure_client_loaded(state).and_then(|client| {
                if force_search {
                    crate::cli::resolve_first_search_track(&client, &query)
                } else {
                    crate::cli::resolve_track_input(&client, &query, true)
                }
            }) {
                Ok(track) => track,
                Err(err) => return DaemonResponse::Error(err.to_string()),
            };
            // Same call the StartRadioFromSeed arm makes for CLI `exec`
            // (play_track_with_radio): singleton seed queue, play immediately.
            match start_radio_from_seed(
                state,
                vec![track],
                0,
                0,
                1,
                None,
                RepeatMode::Off,
                true,
                false,
            ) {
                Ok(()) => {
                    maybe_persist_playback_state(state, true);
                    DaemonResponse::State(state_snapshot(state))
                }
                Err(err) => DaemonResponse::Error(err.to_string()),
            }
        }
        DaemonRequest::GetLyrics => {
            let Some(video_id) = state
                .active_track
                .as_ref()
                .map(|track| track.video_id.clone())
            else {
                return DaemonResponse::Error("Nothing is playing".to_string());
            };

            if let Some((cached_video_id, lyrics)) = state.lyrics_cache.as_ref() {
                if *cached_video_id == video_id {
                    return DaemonResponse::Lyrics(lyrics.clone());
                }
            }

            match ensure_client_loaded(state).and_then(|client| client.get_synced_lyrics(&video_id))
            {
                Ok(lyrics) => {
                    // Cache "no lyrics" results too, but never failures.
                    state.lyrics_cache = Some((video_id, lyrics.clone()));
                    DaemonResponse::Lyrics(lyrics)
                }
                Err(err) => DaemonResponse::Error(err.to_string()),
            }
        }
        DaemonRequest::ClearQueue => {
            stop_playback(state, false);
            state.probe_context = None;
            state.queue.clear();
            state.queue_index = 0;
            state.radio_virtual_playlist = None;
            state.playlist_context = None;
            state.unshuffled_order = None;
            state.radio_append_in_flight = false;
            state.last_radio_append_anchor = None;
            sync_mpris(state);
            maybe_persist_playback_state(state, true);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::StaticfsUnmountNow => {
            #[cfg(feature = "staticfs")]
            {
                state.staticfs_host.unmount_now();
                // The host clears the pending state asynchronously; strip it
                // from the immediate reply so the prompt cannot flicker back.
                let mut snapshot = state_snapshot(state);
                snapshot.staticfs_unmount_pending = None;
                DaemonResponse::State(snapshot)
            }
            #[cfg(not(feature = "staticfs"))]
            DaemonResponse::Error("this build of static does not include staticfs".to_string())
        }
        DaemonRequest::StaticfsUnmountWait => {
            #[cfg(feature = "staticfs")]
            {
                state.staticfs_host.unmount_wait();
                let mut snapshot = state_snapshot(state);
                snapshot.staticfs_unmount_pending = None;
                DaemonResponse::State(snapshot)
            }
            #[cfg(not(feature = "staticfs"))]
            DaemonResponse::Error("this build of static does not include staticfs".to_string())
        }
        DaemonRequest::Shutdown => {
            request_daemon_shutdown(state, true);
            DaemonResponse::Ok
        }
    }
}

fn handle_background_event(state: &mut DaemonState, event: BackgroundEvent) {
    match event {
        BackgroundEvent::TrackingReady {
            video_id,
            attempt,
            tracking,
        } => {
            let tracking_ready = tracking.is_some();
            if let Some(active) = state.active_track.as_mut() {
                if active.video_id == video_id && active.tracking.is_none() {
                    active.tracking = tracking;
                }
            }

            let still_needs_tracking = state
                .active_track
                .as_ref()
                .is_some_and(|track| track.video_id == video_id && track.tracking.is_none());
            if let Some(retry_delay) =
                tracking_retry_delay(tracking_ready, still_needs_tracking, attempt)
            {
                // The failed attempt may have used a stale auth client; drop
                // it so the retry bootstraps from a freshly built one.
                state.client = None;
                state.client_cookie_fingerprint = None;

                let next_attempt = attempt + 1;
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
        BackgroundEvent::PrefetchFinished { video_id } => {
            state.prefetch_in_flight.remove(&video_id);
        }
        BackgroundEvent::ConnectivityProbeFinished { online } => {
            state.connectivity_probe_in_flight = false;
            state.online = online;
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
        BackgroundEvent::SinkSampleReady { sample } => {
            state.desktop_volume_io_in_flight = false;
            let comp_before = state.desktop_volume.comp_gain();
            state.desktop_volume.observe_sample(sample);
            if !volumes_match(comp_before, state.desktop_volume.comp_gain()) {
                apply_desktop_volume_comp(state);
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
    poll_pipewire_streams(state);
    // Before any resume/unduck below, so the compensated steady level is
    // already what those paths ramp or jump to.
    tick_desktop_aware_volume(state);
    maybe_resume_after_external_mpris(state);
    maybe_pause_for_external_mpris(state);
    maybe_unduck_for_pipewire(state);
    maybe_duck_for_pipewire(state);
    maybe_probe_connectivity(state);
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

    maybe_start_pending_playback(state, Instant::now());

    if should_count_paused_idle_for_shutdown(
        has_current_track(state),
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
        // Radio-shuffle pick applies to natural advancement too; repeat-One and
        // repeat-All prefix-loop targets fall inside the touched region and pass
        // through untouched.
        let target = match promote_random_untouched_pick(
            state,
            natural_end_target_index(queue_len, state.queue_index, repeat_mode),
        ) {
            Ok(target) => target,
            Err(err) => {
                let insert_at = state.queue_index.saturating_add(1);
                if seamless_cover(state, insert_at, "no upcoming radio song is cached") {
                    return;
                }
                // Offline with nothing cached: stopping is the only honest
                // outcome for an unattended track end (the daemon has no push
                // error channel; user-initiated Next surfaces the message
                // through `DaemonResponse::Error`).
                eprintln!("[DAEMON] cannot advance at track end: {err}");
                stop_playback(state, false);
                return;
            }
        };
        let cover_attempted = seamless_should_cover_target(state, target);
        if cover_attempted {
            let insert_at = state.queue_index.saturating_add(1);
            if seamless_cover(state, insert_at, "the next song cannot be fetched") {
                return;
            }
        }
        match target {
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
                    if !cover_attempted {
                        let insert_at = seamless_insert_slot_after_failed_target(
                            state.online,
                            state.queue_index.saturating_add(1),
                            target,
                        );
                        seamless_cover(state, insert_at, "the next song failed to load");
                    }
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
    invalidate_unshuffled_order_for_new_queue(state, &queue);
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

fn play_queue_at_for_user_action(
    state: &mut DaemonState,
    queue: Vec<Track>,
    index: usize,
    playlist_context: Option<PlaylistContext>,
    radio_virtual_playlist: Option<RadioVirtualPlaylist>,
    repeat_mode: RepeatMode,
    start_mode: PlaybackStartMode,
    suppress_reporting: bool,
) -> Result<()> {
    if !should_desync_playback_start(start_mode, suppress_reporting) {
        return play_queue_at(
            state,
            queue,
            index,
            playlist_context,
            radio_virtual_playlist,
            repeat_mode,
            start_mode,
            suppress_reporting,
        );
    }

    if queue.is_empty() {
        stop_playback(state, false);
        return Ok(());
    }

    invalidate_unshuffled_order_for_new_queue(state, &queue);
    state.queue = queue;
    state.queue_index = index.min(state.queue.len().saturating_sub(1));
    state.playlist_context = playlist_context;
    state.radio_virtual_playlist = radio_virtual_playlist;
    state.radio_append_in_flight = false;
    state.last_radio_append_anchor = None;
    state.repeat_mode = repeat_mode;
    stage_play_index_after_desync(
        state,
        state.queue_index,
        0,
        TrackRepeatBudgetMode::ResetForCurrentTrack,
        suppress_reporting,
        None,
    )
}

fn should_desync_playback_start(start_mode: PlaybackStartMode, suppress_reporting: bool) -> bool {
    start_mode == PlaybackStartMode::Playing && !suppress_reporting
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
        resume_playback: has_current_track(state) && !is_logically_paused(state),
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
    state.pending_playback_start = None;
    state.player.stop();
    state.volume_ramp = None;
    state.player = state.player.replacement(&state.settings);
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

fn advance_queue_for_user_action(state: &mut DaemonState, forward: bool) -> Result<()> {
    let mut target = queue_target_index(state, forward);
    if forward {
        let insert_at = state.queue_index.saturating_add(1);
        target = match promote_random_untouched_pick(state, target) {
            Ok(target) => target,
            Err(err) => Some(
                seamless_fill_at(state, insert_at, "no upcoming radio song is cached")
                    .ok_or(err)?,
            ),
        };
        if seamless_should_cover_target(state, target) {
            if let Some(index) =
                seamless_fill_at(state, insert_at, "the next song cannot be fetched")
            {
                target = Some(index);
            }
        }
    }
    match target {
        Some(index) => {
            pause_other_mpris_players(state);
            stage_play_index_after_desync(
                state,
                index,
                0,
                TrackRepeatBudgetMode::ResetForCurrentTrack,
                false,
                None,
            )?;
            if let Some(pending) = state.pending_playback_start.as_mut() {
                pending.forward_advance = forward;
            }
            Ok(())
        }
        None => {
            stop_playback(state, false);
            Ok(())
        }
    }
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

/// Two shuffle semantics, chosen by queue kind (a visible toggle must round-trip,
/// unlike the one-way pre-submit shuffle the Play All buttons do):
///
/// - Playlist queues: classic eager shuffle of the unplayed tail, with the pre-shuffle
///   order snapshotted so disabling actually unshuffles.
/// - Radio queues: the order is never rearranged and `touched_len` stays put; instead
///   each forward advance picks a *random* untouched track (see
///   [`promote_random_untouched_pick`]). This composes with the endlessly expanding
///   radio tail — new appends are immediately eligible — and disabling is trivially
///   lossless.
fn set_shuffle(state: &mut DaemonState, enabled: bool) {
    if state.shuffle == enabled {
        return;
    }

    if enabled && state.queue.len() > 1 && state.radio_virtual_playlist.is_none() {
        state.unshuffled_order = Some(state.queue.clone());
        let tail_start = state.queue_index.saturating_add(1).min(state.queue.len());
        use rand::seq::SliceRandom;
        state.queue[tail_start..].shuffle(&mut rand::rng());
    }

    if !enabled && let Some(original) = state.unshuffled_order.take() {
        let tail_start = state.queue_index.saturating_add(1).min(state.queue.len());
        if tail_start < state.queue.len() {
            // History and the current track stay anchored; only the unplayed tail
            // returns to its pre-shuffle relative order (mid-shuffle appends keep
            // their spot at the end).
            let tail = state.queue.split_off(tail_start);
            state.queue.extend(restore_order(tail, &original));
        }
    }

    state.shuffle = enabled;
    // Defensive clamp only: the ordinary advance bookkeeping already marks the playing
    // index touched, so this never grows `touched_len` in practice.
    mark_radio_touched_through(state, state.queue_index);
    state.last_radio_append_anchor = None;
    prefetch_upcoming(state);
    sync_mpris(state);
}

/// Reorders `tracks` to their relative order in `original` (matched by video id,
/// duplicates positionally). Tracks absent from `original` — e.g. tracks queued while
/// shuffled — keep their current relative order after all known tracks. Stable, so
/// disabling shuffle restores the pre-shuffle listening order without inventing one
/// for newcomers.
fn restore_order(tracks: Vec<Track>, original: &[Track]) -> Vec<Track> {
    let mut positions: HashMap<&str, VecDeque<usize>> = HashMap::new();
    for (index, track) in original.iter().enumerate() {
        positions
            .entry(track.video_id.as_str())
            .or_default()
            .push_back(index);
    }

    let mut keyed: Vec<(usize, Track)> = tracks
        .into_iter()
        .map(|track| {
            let position = positions
                .get_mut(track.video_id.as_str())
                .and_then(VecDeque::pop_front)
                .unwrap_or(usize::MAX);
            (position, track)
        })
        .collect();
    keyed.sort_by_key(|(position, _)| *position);
    keyed.into_iter().map(|(_, track)| track).collect()
}

// Radio-shuffle pick distribution: a floor keeps every candidate reachable so
// low-probability candidates stay pickable. Recomputed at every advance from the
// live history so listening stays fresh:
//   0.5  creator repulsion — candidates by recently played artists are pushed down
//   0.5  title repulsion   — "a", "a - remix", "a (nightcore)" variants of recent titles

const RADIO_PICK_CREATOR_WEIGHT: f64 = 0.5;
const RADIO_PICK_TITLE_WEIGHT: f64 = 0.5;

/// History-aware repulsion decay: every song of distance from the latest played track
/// weakens that track's repulsive force by this step (just-played = full force 1.0, one
/// before = 0.95, ...). Forces reach zero after 1/decay songs, so scanning the last
/// ceil(1/decay) history entries is exactly equivalent to scanning the entire history.
const RADIO_PICK_REPULSION_DECAY: f64 = 0.05;

/// Weight floor so no candidate is ever unpickable.
const RADIO_PICK_MIN_WEIGHT: f64 = 0.02;

/// Per-candidate categorical weights for the radio-shuffle pick. `history` is the
/// played prefix in play order (current track last); every history track repels, with
/// force decaying by [`RADIO_PICK_REPULSION_DECAY`] per song of age.
fn radio_pick_weights(candidates: &[Track], history: &[Track]) -> Vec<f64> {
    let horizon = (1.0 / RADIO_PICK_REPULSION_DECAY).ceil() as usize;
    let recent = &history[history.len().saturating_sub(horizon)..];
    candidates
        .iter()
        .map(|candidate| {
            let candidate_artists = artist_keys(candidate);
            let mut creator_penalty: f64 = 0.0;
            let mut title_penalty: f64 = 0.0;
            for (index, played) in recent.iter().enumerate() {
                let force =
                    (1.0 - RADIO_PICK_REPULSION_DECAY * (recent.len() - 1 - index) as f64).max(0.0);
                if !candidate_artists.is_empty()
                    && !candidate_artists.is_disjoint(&artist_keys(played))
                {
                    creator_penalty = creator_penalty.max(force);
                }
                title_penalty = title_penalty.max(force * title_similarity(candidate, played));
            }
            (RADIO_PICK_CREATOR_WEIGHT * (1.0 - creator_penalty)
                + RADIO_PICK_TITLE_WEIGHT * (1.0 - title_penalty))
                .max(RADIO_PICK_MIN_WEIGHT)
        })
        .collect()
}

/// Samples a categorical index from `weights` using `roll` in `[0, 1)` — always a
/// sample, never a maximiser. A zero total falls back to a uniform pick.
fn weighted_index(weights: &[f64], roll: f64) -> usize {
    assert!(!weights.is_empty(), "Cannot sample from empty weights");
    let total: f64 = weights.iter().sum();
    if total <= 0.0 {
        return ((roll * weights.len() as f64) as usize).min(weights.len() - 1);
    }
    let mut remaining = roll.clamp(0.0, 1.0 - 1e-12) * total;
    for (index, weight) in weights.iter().enumerate() {
        remaining -= weight;
        if remaining < 0.0 {
            return index;
        }
    }
    weights.len() - 1
}

/// Similarity in `[0, 1]` tuned for release-variant detection across real YouTube
/// titles ("UNETHICAL" vs "Faouzia - Unethical (MAPHRA Vocal Cover)"). Both titles
/// are reduced to core tokens by stripping the union of BOTH tracks' artist-name
/// tokens (uploader formats carry the original artist in the title while the
/// uploader holds the credit — only the union removes the prefix) plus the
/// [`DESCRIPTOR_TOKENS`] stoplist. Non-empty cores compare by containment (the
/// smaller core fully inside the larger → 1.0) then Jaccard; a degenerate empty
/// core falls back to base-title equality then raw-token Jaccard.
fn title_similarity(a: &Track, b: &Track) -> f64 {
    if a.title.trim().is_empty() || b.title.trim().is_empty() {
        return 0.0;
    }
    let mut strip = artist_title_tokens(a);
    strip.extend(artist_title_tokens(b));
    let core = |title: &str| -> HashSet<String> {
        title_tokens(title)
            .into_iter()
            .filter(|token| !strip.contains(token) && !DESCRIPTOR_TOKENS.contains(&token.as_str()))
            .collect()
    };
    let core_a = core(&a.title);
    let core_b = core(&b.title);
    if !core_a.is_empty() && !core_b.is_empty() {
        let shared = core_a.intersection(&core_b).count();
        if shared == core_a.len().min(core_b.len()) {
            return 1.0;
        }
        return shared as f64 / (core_a.len() + core_b.len() - shared) as f64;
    }
    let base_a = base_title(&a.title);
    if !base_a.is_empty() && base_a == base_title(&b.title) {
        return 1.0;
    }
    let tokens_a = title_tokens(&a.title);
    let tokens_b = title_tokens(&b.title);
    if tokens_a.is_empty() || tokens_b.is_empty() {
        return 0.0;
    }
    let shared = tokens_a.intersection(&tokens_b).count() as f64;
    shared / (tokens_a.len() as f64 + tokens_b.len() as f64 - shared)
}

fn artist_title_tokens(track: &Track) -> HashSet<String> {
    track
        .artists
        .iter()
        .flat_map(|artist| title_tokens(&artist.name))
        .collect()
}

/// Production/upload descriptors that say how a recording was made or packaged,
/// never which song it is. Instrument and genre words are deliberately absent —
/// they appear in genuine song names ("Piano Man") and stripping them would fuse
/// unrelated titles.
const DESCRIPTOR_TOKENS: &[&str] = &[
    "remix",
    "mix",
    "flip",
    "mashup",
    "bootleg",
    "edit",
    "extended",
    "version",
    "ver",
    "cover",
    "covered",
    "karaoke",
    "instrumental",
    "acoustic",
    "unplugged",
    "live",
    "performance",
    "session",
    "sessions",
    "orchestral",
    "orchestra",
    "symphonic",
    "cinematic",
    "emotional",
    "epic",
    "nightcore",
    "sped",
    "slowed",
    "reverb",
    "8d",
    "remaster",
    "remastered",
    "lyric",
    "lyrics",
    "lyrical",
    "official",
    "video",
    "audio",
    "music",
    "mv",
    "visualizer",
    "visualiser",
    "hq",
    "hd",
    "4k",
    "8k",
    "vocal",
    "vocals",
    "feat",
    "ft",
    "featuring",
    "prod",
    "by",
];

fn base_title(title: &str) -> String {
    let lower = title.to_lowercase().trim().to_string();
    let cut = [" - ", " – ", " — ", "(", "[", "|", " // ", " ~"]
        .iter()
        .filter_map(|delimiter| lower.find(delimiter).filter(|index| *index > 0))
        .min()
        .unwrap_or(lower.len());
    lower[..cut]
        .trim()
        .trim_end_matches(['-', '~', '–'])
        .to_string()
}

fn title_tokens(title: &str) -> HashSet<String> {
    title
        .to_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

fn artist_keys(track: &Track) -> HashSet<String> {
    track
        .artists
        .iter()
        .filter_map(|artist| {
            let name = artist.name.trim().to_lowercase();
            (!name.is_empty()).then_some(name)
        })
        .collect()
}

/// Radio-shuffle pick core: when a forward advance target lands in the untouched
/// region of a shuffled radio queue, move the candidate chosen by `pick_offset`
/// (called with the untouched candidates `[touched_len, queue_len)` and the played
/// history `[0, queue_index]`, returning an offset into the candidates) to the target
/// position. Targets inside the touched prefix (repeat-All prefix loops, repeat-One,
/// forward through replayed history) pass through untouched, as do playlist queues
/// (`radio_touched_len` = None). Returns whether a track actually moved.
fn promote_untouched_pick(
    queue: &mut Vec<Track>,
    shuffle: bool,
    radio_touched_len: Option<usize>,
    queue_index: usize,
    target: Option<usize>,
    pick_offset: impl FnOnce(&[Track], &[Track]) -> usize,
) -> bool {
    let Some(touched_len) = radio_touched_len else {
        return false;
    };
    let Some(target) = target else {
        return false;
    };
    if !shuffle || target < touched_len || target >= queue.len() {
        return false;
    }

    let candidates = &queue[touched_len..];
    let history = &queue[..queue_index.saturating_add(1).min(queue.len())];
    let pick = touched_len + pick_offset(candidates, history);
    if pick == target {
        return false;
    }
    let track = queue.remove(pick);
    queue.insert(target, track);
    true
}

/// Radio-shuffle pick: promote the pre-decided untouched track (see
/// [`DaemonState::shuffle_up_next`]) to the forward advance target (user Next
/// and natural track end alike), then advance into it normally — `touched_len`
/// grows only through the ordinary advance bookkeeping. The sample is
/// categorical, not uniform and not a maximiser — [`radio_pick_weights`]
/// recomputes the distribution once per track (creator repulsion 0.5,
/// similar-title repulsion 0.5, at decision time in
/// [`refresh_shuffle_up_next`]) and every candidate keeps a floor probability.
/// History keeps true play order and the upcoming list keeps its stable order.
/// The consumed pick is cleared — the next track start re-decides it, keeping
/// the per-advance "recalculated every time" semantics. Errs only when offline
/// with nothing cached (see [`resolve_shuffle_advance_pick`]), leaving both the
/// queue and the stored pick untouched. Returns the target unchanged.
fn promote_random_untouched_pick(
    state: &mut DaemonState,
    target: Option<usize>,
) -> Result<Option<usize>> {
    let Some(touched_len) = state
        .radio_virtual_playlist
        .as_ref()
        .map(|radio| radio.touched_len)
    else {
        return Ok(target);
    };
    let Some(target_index) = target else {
        return Ok(target);
    };
    if !state.shuffle || target_index < touched_len || target_index >= state.queue.len() {
        return Ok(target);
    }

    let offset = {
        use rand::RngExt;
        let candidates = &state.queue[touched_len..];
        let history = &state.queue[..state.queue_index.saturating_add(1).min(state.queue.len())];
        resolve_shuffle_advance_pick(
            candidates,
            history,
            state.shuffle_up_next.as_deref(),
            state.online,
            |track| audio_is_cached(&state.cache, &track.video_id),
            rand::rng().random::<f64>(),
        )?
    };
    state.shuffle_up_next = None;
    let moved = promote_untouched_pick(
        &mut state.queue,
        state.shuffle,
        Some(touched_len),
        state.queue_index,
        target,
        |_, _| offset,
    );
    if moved {
        // The tail composition changed; stale append bookkeeping must not block
        // radio extension.
        state.last_radio_append_anchor = None;
    }
    Ok(target)
}

/// Resolves the untouched-region offset a forward radio-shuffle advance
/// promotes: the pre-decided pick when it is still among `candidates` (its
/// prefetch has had the whole track to land), else a fresh weighted sample.
/// Offline (and only offline), the sampleable pool shrinks to candidates whose
/// audio is already cached — which normally includes the pre-decided pick
/// itself. With nothing cached the advance fails loudly rather than starting a
/// song that cannot stream.
fn resolve_shuffle_advance_pick(
    candidates: &[Track],
    history: &[Track],
    predecided_id: Option<&str>,
    online: bool,
    is_cached: impl Fn(&Track) -> bool,
    roll: f64,
) -> Result<usize> {
    let predecided =
        predecided_id.and_then(|id| candidates.iter().position(|track| track.video_id == id));
    if online {
        return Ok(predecided
            .unwrap_or_else(|| weighted_index(&radio_pick_weights(candidates, history), roll)));
    }
    if let Some(offset) = predecided
        && is_cached(&candidates[offset])
    {
        return Ok(offset);
    }
    let cached_mask: Vec<bool> = candidates.iter().map(&is_cached).collect();
    cached_restricted_pick_offset(candidates, history, &cached_mask, roll)
        .ok_or_else(|| anyhow!("You're offline and none of the upcoming songs are cached"))
}

/// Offline radio-shuffle fallback: the weighted sample restricted to candidates
/// whose audio is already cached (`cached_mask` aligned with `candidates`).
/// Weights are recomputed over the cached subset only, so repulsion still
/// shapes the restricted distribution. Returns the offset within `candidates`,
/// or None when nothing startable is cached — the caller's cue to fail loudly
/// instead of picking a song that cannot play.
fn cached_restricted_pick_offset(
    candidates: &[Track],
    history: &[Track],
    cached_mask: &[bool],
    roll: f64,
) -> Option<usize> {
    let positions: Vec<usize> = cached_mask
        .iter()
        .enumerate()
        .filter_map(|(index, &cached)| cached.then_some(index))
        .collect();
    if positions.is_empty() {
        return None;
    }
    let cached_candidates: Vec<Track> = positions
        .iter()
        .map(|&position| candidates[position].clone())
        .collect();
    let weights = radio_pick_weights(&cached_candidates, history);
    Some(positions[weighted_index(&weights, roll)])
}

/// Whether `video_id` can start without the network: the permanent audio cache
/// or the temp prefetch cache — the same stores [`prepare_playback`] checks
/// before resolving a remote stream.
fn audio_is_cached(cache: &MusicCache, video_id: &str) -> bool {
    cache.is_cached(video_id) || cache.get_temp_audio(video_id).is_some()
}

// Seamless listening: when a forward advance cannot fetch its next song, the
// daemon bridges the gap with a pick from the offline library instead of
// stopping. Fills are ordinary queue entries inserted next to the current
// track (so they land inside the radio touched prefix), flagged
// `Track::offline_fill` so frontends can tint them. Explicit song choices are
// never substituted — only forward advances are covered.

/// Played-history window the seamless pick avoids repeating once every offline
/// song is already somewhere in the queue.
const SEAMLESS_RECENT_HISTORY: usize = 20;

/// Whether a forward advance is covered from the offline library up front,
/// without trying the network. `target_available` is whether the advance
/// target can start locally (None = the queue has no next song). A reachable
/// network is always tried first; its failures are covered afterwards by the
/// callers' error paths.
fn seamless_covers_advance(
    enabled: bool,
    online: bool,
    target_available: Option<bool>,
    radio_ran_dry: bool,
) -> bool {
    if !enabled {
        return false;
    }
    match target_available {
        Some(available) => !online && !available,
        None => radio_ran_dry,
    }
}

/// A radio queue is endless by contract, so reaching its end means the tail
/// could not be extended. While the extension is still in flight the advance
/// waits for it, unless the network is known to be down.
fn radio_ran_dry(
    has_radio_queue: bool,
    repeat_mode: RepeatMode,
    queue_len: usize,
    radio_append_in_flight: bool,
    online: bool,
) -> bool {
    has_radio_queue
        && repeat_mode == RepeatMode::Off
        && queue_len > 0
        && (!radio_append_in_flight || !online)
}

fn seamless_should_cover_target(state: &DaemonState, target: Option<usize>) -> bool {
    // Replaying the current track (repeat-One) counts as available: it is not
    // a "next song" to substitute.
    let target_available = target.map(|index| {
        index == state.queue_index
            || state
                .queue
                .get(index)
                .is_some_and(|track| audio_is_cached(&state.cache, &track.video_id))
    });
    seamless_covers_advance(
        state.settings.seamless_listening,
        state.online,
        target_available,
        radio_ran_dry(
            state.radio_virtual_playlist.is_some(),
            state.repeat_mode,
            state.queue.len(),
            state.radio_append_in_flight,
            state.online,
        ),
    )
}

/// Where the fill goes once the advance target at `target` already failed to
/// load. Offline, the failed song stays ahead of the fill (`before_target`) to
/// be retried when the network is back. Online, the song itself is the
/// suspect, so the fill lands behind it and the advance moves past — otherwise
/// one unplayable song would be retried forever.
fn seamless_insert_slot_after_failed_target(
    online: bool,
    before_target: usize,
    target: usize,
) -> usize {
    if online {
        target.saturating_add(1)
    } else {
        before_target
    }
}

/// Per-candidate weights for the seamless pick: how much the user plays the
/// song (its offline score, log-damped so favourites lead without drowning
/// the rest) times the radio pick's freshness repulsion against `history`.
fn seamless_pick_weights(candidates: &[Track], scores: &[f64], history: &[Track]) -> Vec<f64> {
    radio_pick_weights(candidates, history)
        .into_iter()
        .zip(scores)
        .map(|(freshness, score)| freshness * (1.0 + score.max(0.0).ln_1p()))
        .collect()
}

/// "Which song would you like next" over the offline library (`library` pairs
/// each playable offline track with its offline score): a weighted sample via
/// [`seamless_pick_weights`]. Songs already in the queue are skipped; when the
/// whole library is queued the pool widens step by step — outside the recent
/// history, then anything but the current song, then anything at all.
fn seamless_pick(
    library: &[(Track, f64)],
    queue: &[Track],
    queue_index: usize,
    roll: f64,
) -> Option<Track> {
    let history = &queue[..queue_index.saturating_add(1).min(queue.len())];
    let queued: HashSet<&str> = queue.iter().map(|track| track.video_id.as_str()).collect();
    let recent: HashSet<&str> = history[history.len().saturating_sub(SEAMLESS_RECENT_HISTORY)..]
        .iter()
        .map(|track| track.video_id.as_str())
        .collect();
    let current = history.last().map(|track| track.video_id.as_str());

    let eligible = |allowed: &dyn Fn(&str) -> bool| -> Vec<&(Track, f64)> {
        library
            .iter()
            .filter(|(track, _)| allowed(&track.video_id))
            .collect()
    };
    let mut pool = eligible(&|id| !queued.contains(id));
    if pool.is_empty() {
        pool = eligible(&|id| !recent.contains(id));
    }
    if pool.is_empty() {
        pool = eligible(&|id| Some(id) != current);
    }
    if pool.is_empty() {
        pool = library.iter().collect();
    }
    if pool.is_empty() {
        return None;
    }

    let candidates: Vec<Track> = pool.iter().map(|(track, _)| track.clone()).collect();
    let scores: Vec<f64> = pool.iter().map(|(_, score)| *score).collect();
    let weights = seamless_pick_weights(&candidates, &scores, history);
    Some(candidates[weighted_index(&weights, roll)].clone())
}

/// The combined offline store (automatic, manual, and playlist downloads) as
/// queueable tracks with their offline scores.
fn seamless_library(state: &DaemonState) -> Vec<(Track, f64)> {
    let tracks = match state
        .cache
        .scored_offline_tracks(state.settings.score_decay_window_days)
    {
        Ok(tracks) => tracks,
        Err(err) => {
            eprintln!("[SEAMLESS] failed to read the offline library: {err}");
            return Vec::new();
        }
    };
    tracks
        .into_iter()
        .map(|offline| {
            let track = Track {
                video_id: offline.video_id,
                title: offline.title,
                artists: if offline.artist.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![Artist {
                        name: offline.artist,
                        id: None,
                        page_type: None,
                    }]
                },
                album: None,
                duration: None,
                thumbnail_url: offline
                    .art_path
                    .filter(|path| std::path::Path::new(path).exists())
                    .map(|path| format!("file://{path}")),
                playback_playlist_id: None,
                playback_params: None,
                not_interested_feedback_token: None,
                undo_feedback_token: None,
                offline_fill: true,
            };
            (track, offline.score)
        })
        .collect()
}

/// Inserts a seamless pick at `insert_at` and returns its queue index, or None
/// when the feature is off or the offline library is empty. Does not start
/// playback.
fn seamless_fill_at(state: &mut DaemonState, insert_at: usize, reason: &str) -> Option<usize> {
    if !state.settings.seamless_listening {
        return None;
    }
    let roll = {
        use rand::RngExt;
        rand::rng().random::<f64>()
    };
    let library = seamless_library(state);
    let Some(track) = seamless_pick(&library, &state.queue, state.queue_index, roll) else {
        eprintln!("[SEAMLESS] {reason}, but the offline library is empty");
        return None;
    };
    let index = insert_at.min(state.queue.len());
    eprintln!(
        "[SEAMLESS] {reason}; filling queue slot {} with offline song {} ({})",
        index.saturating_add(1),
        track.video_id,
        track.title
    );
    insert_into_queue(state, vec![track], index);
    Some(index)
}

/// [`seamless_fill_at`] plus starting the fill. Returns whether a fill is now
/// playing.
fn seamless_cover(state: &mut DaemonState, insert_at: usize, reason: &str) -> bool {
    let Some(index) = seamless_fill_at(state, insert_at, reason) else {
        return false;
    };
    match play_index(state, index) {
        Ok(()) => {
            maybe_persist_playback_state(state, true);
            true
        }
        Err(err) => {
            eprintln!("[SEAMLESS] failed to start the offline fill: {err}");
            // Leave no trace of a fill that never played, so a caller that
            // falls back to its own handling sees the queue it started with.
            if let Err(err) = remove_from_queue(state, index) {
                eprintln!("[SEAMLESS] failed to withdraw the unplayable fill: {err}");
            }
            false
        }
    }
}

/// Decides (or re-validates) the radio-shuffle pick for the NEXT advance. A
/// still-valid pick is kept — its prefetch may already be underway — while an
/// absent or invalidated one (consumed by an advance, removed from the queue,
/// shuffle/radio turned off) is re-sampled from the current untouched region,
/// keeping the per-track "recalculated every time" semantics of the original
/// advance-time sample.
fn refresh_shuffle_up_next(state: &mut DaemonState) {
    use rand::RngExt;
    state.shuffle_up_next = refresh_shuffle_up_next_id(
        &state.queue,
        state.shuffle,
        state
            .radio_virtual_playlist
            .as_ref()
            .map(|radio| radio.touched_len),
        state.queue_index,
        state.shuffle_up_next.take(),
        rand::rng().random::<f64>(),
    );
}

/// Pure core of [`refresh_shuffle_up_next`]: `current` survives while still in
/// the untouched region `[touched_len, queue_len)`; otherwise `roll` samples a
/// fresh pick from [`radio_pick_weights`] over the untouched candidates
/// (history = the queue up to and including `queue_index`). Shuffle off, no
/// radio, or an empty untouched region clear the pick.
fn refresh_shuffle_up_next_id(
    queue: &[Track],
    shuffle: bool,
    radio_touched_len: Option<usize>,
    queue_index: usize,
    current: Option<String>,
    roll: f64,
) -> Option<String> {
    let touched_len =
        radio_touched_len.filter(|&touched_len| shuffle && touched_len < queue.len())?;
    let candidates = &queue[touched_len..];
    if let Some(id) = current
        && candidates.iter().any(|track| track.video_id == id)
    {
        return Some(id);
    }
    let history = &queue[..queue_index.saturating_add(1).min(queue.len())];
    let weights = radio_pick_weights(candidates, history);
    Some(candidates[weighted_index(&weights, roll)].video_id.clone())
}

/// Queue-jump commands (queue row taps, resume restores) resubmit the live queue at a
/// new index; only a genuinely different track-id sequence invalidates the unshuffle
/// snapshot.
fn invalidate_unshuffled_order_for_new_queue(state: &mut DaemonState, queue: &[Track]) {
    let same_queue = state.queue.len() == queue.len()
        && state
            .queue
            .iter()
            .zip(queue)
            .all(|(current, incoming)| current.video_id == incoming.video_id);
    if !same_queue {
        state.unshuffled_order = None;
    }
}

fn hide_timing_for_track(track: &ActiveTrack) -> bool {
    track.awaiting_fresh_position && track.startup_seek_millis.is_none()
}

fn visible_position_millis(state: &DaemonState) -> Option<u64> {
    if state.active_track.is_none() && state.pending_playback_start.is_some() {
        return Some(0);
    }

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
    if state.active_track.is_none() && state.pending_playback_start.is_some() {
        return Some(0);
    }

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
    if state.active_track.is_none() && state.pending_playback_start.is_some() {
        return Some(0);
    }

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
    if state.active_track.is_none() {
        if let Some(pending) = state.pending_playback_start.as_ref() {
            return !pending.start_when_due;
        }
    }

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
    if state.active_track.is_none() && state.pending_playback_start.is_some() {
        return Some(0);
    }

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
    if state.active_track.is_none() {
        if let Some(pending) = state.pending_playback_start.as_mut() {
            pending.start_when_due = false;
            state.volume_ramp = None;
            return;
        }
    }

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
    if state.active_track.is_none() {
        if let Some(pending) = state.pending_playback_start.as_mut() {
            pending.start_when_due = true;
            pending.due_at = Instant::now();
            state.sleep_shutdown_pending = false;
            return;
        }
    }

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

    // Known-offline remote starts cannot recover by retrying; hand over to the
    // failure path right away so seamless listening can bridge the gap.
    let retry_is_pointless = state.settings.seamless_listening
        && !state.online
        && playback_source == PlaybackSource::RemoteStream;

    if startup_retries < PLAYBACK_START_RETRY_LIMIT && !retry_is_pointless {
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
    let action = previous_action(
        queue_len,
        state.queue_index,
        state.repeat_mode,
        position_millis,
        has_current_track(state),
    );

    match action {
        PreviousAction::RestartCurrent => {
            if state.probe_context.is_some() {
                seek_to_position(state, ClockTime::ZERO);
                Ok(())
            } else {
                pause_other_mpris_players(state);
                let suppress_reporting = state
                    .active_track
                    .as_ref()
                    .map(|track| !track.reporting_enabled)
                    .or_else(|| {
                        state
                            .pending_playback_start
                            .as_ref()
                            .map(|pending| pending.suppress_reporting)
                    })
                    .unwrap_or(false);
                let preserved_track_repeat_loops_left = state.track_repeat_loops_left;
                stage_play_index_after_desync(
                    state,
                    state.queue_index,
                    0,
                    TrackRepeatBudgetMode::PreserveCurrent,
                    suppress_reporting,
                    preserved_track_repeat_loops_left,
                )
            }
        }
        PreviousAction::PlayIndex(index) => {
            pause_other_mpris_players(state);
            stage_play_index_after_desync(
                state,
                index,
                0,
                TrackRepeatBudgetMode::ResetForCurrentTrack,
                false,
                None,
            )
        }
        PreviousAction::Noop => Ok(()),
    }
}

fn toggle_pause_for_user_action(state: &mut DaemonState) {
    if resume_pending_mpris_halt_auto(state, PlaybackTransition::Immediate, "UI play/pause") {
        return;
    }

    if is_logically_paused(state) && has_current_track(state) {
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
    if has_current_track(state) {
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

fn stage_play_index_after_desync(
    state: &mut DaemonState,
    index: usize,
    startup_retries: u8,
    budget_mode: TrackRepeatBudgetMode,
    suppress_reporting: bool,
    preserved_track_repeat_loops_left: Option<u32>,
) -> Result<()> {
    let track = state
        .queue
        .get(index)
        .cloned()
        .ok_or_else(|| anyhow!("Queue index {index} is out of bounds"))?;
    stop_playback_without_mpris_sync(state, false);
    state.player = state.player.replacement(&state.settings);
    state.probe_context = None;
    state.queue_index = index;
    mark_radio_touched_through(state, index);
    state.last_duration_micros.set(None);
    state.paused_since = None;
    state.pending_playback_start = Some(PendingPlaybackStart {
        due_at: Instant::now() + DESYNC_PLAYBACK_START_DELAY,
        start_when_due: true,
        queue_index: index,
        video_id: track.video_id.clone(),
        startup_retries,
        budget_mode,
        suppress_reporting,
        preserved_track_repeat_loops_left,
        forward_advance: false,
    });
    match budget_mode {
        TrackRepeatBudgetMode::ResetForCurrentTrack => {
            reset_track_repeat_budget_for_current_track(state)
        }
        TrackRepeatBudgetMode::PreserveCurrent => {
            state.track_repeat_loops_left = preserved_track_repeat_loops_left;
        }
    }
    eprintln!(
        "[DAEMON] staged playback for {} after {} ms desync window",
        track.video_id,
        DESYNC_PLAYBACK_START_DELAY.as_millis()
    );
    signal_mpris_seeked(state, ClockTime::ZERO);
    sync_mpris(state);
    maybe_persist_playback_state(state, true);
    Ok(())
}

fn pending_playback_start_is_ready(due_at: Instant, start_when_due: bool, now: Instant) -> bool {
    start_when_due && now >= due_at
}

fn resolve_pending_playback_index(
    queue: &[Track],
    pending_index: usize,
    pending_video_id: &str,
) -> Option<usize> {
    if queue.is_empty() {
        return None;
    }

    let fallback_index = pending_index.min(queue.len().saturating_sub(1));
    if queue
        .get(fallback_index)
        .is_some_and(|track| track.video_id == pending_video_id)
    {
        return Some(fallback_index);
    }

    queue
        .iter()
        .position(|track| track.video_id == pending_video_id)
        .or(Some(fallback_index))
}

fn maybe_start_pending_playback(state: &mut DaemonState, now: Instant) -> bool {
    let Some(pending) = state.pending_playback_start.as_ref() else {
        return false;
    };
    if !pending_playback_start_is_ready(pending.due_at, pending.start_when_due, now) {
        return false;
    }
    let forward_advance = pending.forward_advance;

    if let Err(err) = start_pending_playback(state) {
        eprintln!("[DAEMON] pending playback start failed: {err}");
        if forward_advance {
            let insert_at = seamless_insert_slot_after_failed_target(
                state.online,
                state.queue_index,
                state.queue_index,
            );
            if seamless_cover(state, insert_at, "the next song failed to load") {
                return true;
            }
        }
        stop_playback(state, false);
    }
    true
}

fn start_pending_playback(state: &mut DaemonState) -> Result<()> {
    let Some(pending) = state.pending_playback_start.take() else {
        return Ok(());
    };
    let Some(index) =
        resolve_pending_playback_index(&state.queue, pending.queue_index, &pending.video_id)
    else {
        return Ok(());
    };
    let track = state
        .queue
        .get(index)
        .cloned()
        .ok_or_else(|| anyhow!("Queue index {index} is out of bounds"))?;
    let prepared = prepare_playback(state, &track.video_id)?;
    state.player = state.player.replacement(&state.settings);
    state.probe_context = None;
    start_prepared_playback(
        state,
        index,
        track,
        prepared,
        pending.startup_retries,
        pending.budget_mode,
        PlaybackStartMode::Playing,
        pending.suppress_reporting,
        pending.preserved_track_repeat_loops_left,
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

    stop_playback_without_mpris_sync(state, false);
    state.player = state.player.replacement(&state.settings);
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
    stop_playback_inner(state, exiting, true);
}

fn stop_playback_without_mpris_sync(state: &mut DaemonState, exiting: bool) {
    stop_playback_inner(state, exiting, false);
}

fn stop_playback_inner(state: &mut DaemonState, exiting: bool, refresh_mpris: bool) {
    maybe_persist_playback_state(state, true);
    clear_mpris_halt_auto_resume_pending(state);
    state.pending_playback_start = None;
    state.deferred_actions.clear();
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
    if refresh_mpris {
        sync_mpris(state);
    }
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
    // Seamless listening exists to avoid the network: its fills, and anything
    // started while known-offline, play from local audio regardless of the
    // prefer-local preference.
    let seamless_local = (state.settings.seamless_listening && !state.online)
        || state
            .queue
            .iter()
            .any(|track| track.offline_fill && track.video_id == video_id);
    if state.settings.prefer_local_cache_on_playback || seamless_local {
        if let Some(cached_path) = state.cache.get_cached_audio(video_id) {
            return url::Url::from_file_path(cached_path)
                .map(|url| PreparedPlayback {
                    uri: url.to_string(),
                    source: PlaybackSource::OfflineCache,
                    video_id: video_id.to_string(),
                })
                .map_err(|_| anyhow!("Failed to build file URL for cached audio"));
        }
        if state.settings.prefetch_enabled || seamless_local {
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
            if seamless_cover(state, queue_index, "the cached copy failed to play") {
                return;
            }
            stop_playback(state, false);
        }
        return;
    }

    // A failed stream is the first hint the network dropped; re-probe now
    // instead of waiting out the interval.
    state.last_connectivity_probe_at = None;
    let next_target = queue_target_index(state, true).filter(|target| *target != queue_index);
    if playback_source == PlaybackSource::RemoteStream
        && seamless_should_cover_target(state, next_target)
    {
        // A song that never started is kept ahead of the fill to be retried
        // once the network is back; one that died mid-play stays behind it.
        let insert_at = if awaiting_fresh_position {
            queue_index
        } else {
            queue_index.saturating_add(1)
        };
        if seamless_cover(
            state,
            insert_at,
            "the stream failed and the next song cannot be fetched",
        ) {
            return;
        }
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
                let insert_at = seamless_insert_slot_after_failed_target(
                    state.online,
                    state.queue_index.saturating_add(1),
                    target,
                );
                if seamless_cover(state, insert_at, "the next song failed to load") {
                    return;
                }
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

fn ensure_client_loaded(state: &mut DaemonState) -> Result<Arc<NetworkState>> {
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

    let client = Arc::new(NetworkState::from_cookie(&cookie)?);
    state.client = Some(client.clone());
    state.client_cookie_fingerprint = Some(fingerprint);
    Ok(client)
}

fn reload_settings(state: &mut DaemonState) {
    match state.cache.load_settings() {
        Ok(settings) => {
            let volume_limit_lowered = settings.max_volume() < state.settings.max_volume();
            state.settings = settings;
            state.player.configure_gain(&state.settings);
            if volume_limit_lowered {
                apply_volume_limit_after_settings_reload(state);
            }
            sync_mpris(state);
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
            sync_staticfs_host(state);
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
        } => spawn_playback_tracking(state, video_id, attempt),
    }
}

const TRACKING_MAX_ATTEMPTS: u8 = 3;

fn tracking_retry_delay(
    tracking_ready: bool,
    still_needs_tracking: bool,
    attempt: u8,
) -> Option<Duration> {
    if tracking_ready || !still_needs_tracking || attempt + 1 >= TRACKING_MAX_ATTEMPTS {
        return None;
    }
    Some(Duration::from_secs((attempt as u64 + 1) * 2))
}

/// Bootstraps YouTube watchtime tracking on a background thread. Tracking is
/// best-effort telemetry: it must never hold up the daemon loop, so playback
/// (especially cache-backed playback) starts regardless of network health and
/// the result arrives later as BackgroundEvent::TrackingReady.
fn spawn_playback_tracking(state: &mut DaemonState, video_id: String, attempt: u8) {
    let current_attempt = attempt + 1;
    let session_started_at = state
        .active_track
        .as_ref()
        .filter(|track| track.video_id == video_id)
        .map(|track| track.started_at)
        .unwrap_or_else(Instant::now);

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

    let client = match ensure_client_loaded(state) {
        Ok(client) => Some(client),
        Err(err) => {
            eprintln!(
                "[TRACKING] attempt {current_attempt} for {video_id} could not load auth client: {err}"
            );
            None
        }
    };

    let background_tx = state.background_tx.clone();
    std::thread::spawn(move || {
        let tracking = client.and_then(|client| {
            client.start_playback_tracking(
                &video_id,
                session_started_at,
                playback_playlist_id.as_deref(),
                playback_params.as_deref(),
            )
        });
        match &tracking {
            Some(_) if current_attempt > 1 => eprintln!(
                "[TRACKING] recovered telemetry bootstrap for {video_id} on attempt {current_attempt}"
            ),
            None => eprintln!(
                "[TRACKING] attempt {current_attempt} for {video_id} produced no watchtime tracker"
            ),
            _ => {}
        }
        let _ = background_tx.send(BackgroundEvent::TrackingReady {
            video_id,
            attempt,
            tracking,
        });
    });
}

/// Radio tails top up once this few upcoming tracks remain (was: only on the very
/// last track) so the weighted shuffle always has a pool to sample from and gapless
/// advancement never races the fetch.
const RADIO_EXTEND_MIN_UPCOMING: usize = 5;

fn maybe_extend_radio_queue(state: &mut DaemonState) {
    if state.radio_append_in_flight
        || state.radio_virtual_playlist.is_none()
        || state.repeat_mode == RepeatMode::All
    {
        return;
    }

    let Some(anchor_video_id) = ({
        let upcoming = state
            .queue
            .len()
            .saturating_sub(state.queue_index.saturating_add(1));
        if state.queue.is_empty() || upcoming > RADIO_EXTEND_MIN_UPCOMING {
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

/// Keeps the cached connectivity verdict fresh while an offline-restricted
/// radio-shuffle advance is possible (shuffled radio queue with a current
/// track). The probe itself runs on a background thread; this hot-path hook
/// only checks the throttle and reads/writes daemon state.
fn maybe_probe_connectivity(state: &mut DaemonState) {
    let relevant = has_current_track(state)
        && ((state.settings.seamless_listening && !is_logically_paused(state))
            || (state.shuffle && state.radio_virtual_playlist.is_some()));
    if !relevant
        || state.connectivity_probe_in_flight
        || state
            .last_connectivity_probe_at
            .is_some_and(|probed_at| probed_at.elapsed() < CONNECTIVITY_PROBE_INTERVAL)
    {
        return;
    }

    state.connectivity_probe_in_flight = true;
    state.last_connectivity_probe_at = Some(Instant::now());
    let background_tx = state.background_tx.clone();
    std::thread::spawn(move || {
        let online = probe_connectivity();
        let _ = background_tx.send(BackgroundEvent::ConnectivityProbeFinished { online });
    });
}

/// One short TCP handshake against the streaming endpoint. DNS resolution
/// happens on this background thread too, so a dead resolver counts as offline
/// instead of stalling anything.
fn probe_connectivity() -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let Ok(addrs) = ("music.youtube.com", 443).to_socket_addrs() else {
        return false;
    };
    // Two attempts bound the worst case; the timeout applies per handshake.
    addrs
        .take(2)
        .any(|addr| TcpStream::connect_timeout(&addr, CONNECTIVITY_PROBE_TIMEOUT).is_ok())
}

fn prefetch_upcoming(state: &mut DaemonState) {
    // Every queue mutation and track start funnels through here, so this is the
    // single decision point for the pre-announced shuffle pick — decided BEFORE
    // the prefetch-enabled gate (the pick also feeds the snapshot's "Next up"
    // hint) and BEFORE the candidate list so its download always leads the batch.
    refresh_shuffle_up_next(state);
    if !state.settings.prefetch_enabled {
        return;
    }

    let prefetch_ahead = state.settings.prefetch_ahead.clamp(1, 12) as usize;
    let up_next_index = state
        .shuffle_up_next
        .as_deref()
        .and_then(|id| state.queue.iter().position(|track| track.video_id == id));
    let to_fetch = prefetch_candidate_indices(
        state.queue.len(),
        state.queue_index,
        prefetch_ahead,
        up_next_index,
    )
    .into_iter()
    .filter_map(|index| state.queue.get(index))
    .filter(|track| {
        !audio_is_cached(&state.cache, &track.video_id)
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
            if audio_is_cached(&cache, &video_id) {
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
    up_next_index: Option<usize>,
) -> Vec<usize> {
    if queue_len == 0 {
        return Vec::new();
    }

    let mut indices = Vec::new();
    // The pre-decided radio-shuffle pick downloads first: it is the one song
    // guaranteed to play next, and the offline advance fallback depends on it
    // being cached by the time the current song ends. An out-of-range pick
    // (stale after a queue mutation) is ignored, not crashed on.
    if let Some(up_next) = up_next_index.filter(|&index| index < queue_len) {
        indices.push(up_next);
    }
    if queue_index > 0 {
        indices.push(queue_index - 1);
    }

    let start = queue_index.saturating_add(1);
    indices.extend(start..start.saturating_add(prefetch_ahead).min(queue_len));
    let mut seen = HashSet::new();
    indices.retain(|index| seen.insert(*index));
    indices
}

fn history_thumbnail_candidate(url: Option<&str>) -> Option<&str> {
    url.filter(|value| !value.starts_with("file://"))
}

fn has_current_track(state: &DaemonState) -> bool {
    state.active_track.is_some() || state.pending_playback_start.is_some()
}

fn pending_playback_track(state: &DaemonState) -> Option<&Track> {
    let pending = state.pending_playback_start.as_ref()?;
    let index =
        resolve_pending_playback_index(&state.queue, pending.queue_index, &pending.video_id)?;
    state.queue.get(index)
}

fn pending_playback_track_snapshot(state: &DaemonState) -> Option<ActiveTrackSnapshot> {
    pending_playback_track(state).map(|track| ActiveTrackSnapshot {
        video_id: track.video_id.clone(),
        title: track.title.clone(),
        artist: artist_string(track),
        thumb_url: track.thumbnail_url.clone(),
        rating: SongRating::Indifferent,
    })
}

fn current_track_snapshot(state: &DaemonState) -> Option<ActiveTrackSnapshot> {
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
        .or_else(|| pending_playback_track_snapshot(state))
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

    current_track_snapshot(state).or_else(|| queued_track_snapshot(state))
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
        // The shuffle flag and unshuffle snapshot are daemon-session state; a
        // restored queue starts unshuffled and disabling keeps the current order.
        shuffle: false,
        // The pre-decided pick is daemon-session state like the shuffle flag; a
        // restored queue re-decides on its first track start.
        up_next: None,
        paused: state.probe_context.is_some()
            || (has_current_track(state) && is_logically_paused(state)),
        volume: logical_volume(state),
        max_volume: state.settings.max_volume(),
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
        ducked: false,
        pipewire_pause_active: false,
        desktop_aware_volume: None,
        staticfs_unmount_pending: None,
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
        current_track: current_track_snapshot(state),
        queue: state.queue.clone(),
        queue_index: state.queue_index,
        playlist_context: state.playlist_context.clone(),
        radio_virtual_playlist: state.radio_virtual_playlist.clone(),
        repeat_mode: state.repeat_mode,
        track_repeat_loops_left: state.track_repeat_loops_left,
        shuffle: state.shuffle,
        // Repeat-One and radio prefix loops (repeat-All) never advance into the
        // untouched region, so advertising the pick there would promise a song
        // that won't play next.
        up_next: state
            .shuffle_up_next
            .as_deref()
            .filter(|_| state.repeat_mode == RepeatMode::Off)
            .and_then(|id| {
                state
                    .queue
                    .iter()
                    .find(|track| track.video_id == id)
                    .cloned()
            }),
        paused: is_logically_paused(state),
        volume: logical_volume(state),
        max_volume: state.settings.max_volume(),
        position_millis: (!hide_timing)
            .then(|| visible_position_millis(state))
            .flatten(),
        duration_millis: (!hide_timing)
            .then(|| visible_duration_millis(state))
            .flatten(),
        buffered_fraction: state
            .active_track
            .as_ref()
            .map(|_| state.player.buffered_fraction())
            .unwrap_or(0.0),
        halt_auto_resume_pending: mpris_halt_auto_resume_is_pending(state),
        probe_active,
        probe_restore_position_millis: state
            .probe_context
            .as_ref()
            .and_then(|probe| probe.position_millis),
        sleep_remaining_secs: sleep_status.remaining_secs,
        sleep_after_current: sleep_status.after_current,
        ducked: state.ducked,
        pipewire_pause_active: state.pipewire_pause_active,
        desktop_aware_volume: desktop_volume_snapshot(state),
        #[cfg(feature = "staticfs")]
        staticfs_unmount_pending: state
            .staticfs_host
            .pending_unmount()
            .map(|busy_paths| crate::ipc::StaticfsUnmountPending { busy_paths }),
        #[cfg(not(feature = "staticfs"))]
        staticfs_unmount_pending: None,
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

fn valid_requested_volume(volume: f64, settings: &AppSettings) -> bool {
    volume.is_finite() && (0.0..=settings.max_volume()).contains(&volume)
}

fn apply_volume_limit_after_settings_reload(state: &mut DaemonState) {
    state.pre_duck_volume = state.pre_duck_volume.min(state.settings.max_volume());
    let output = steady_output_volume(state);
    state.player.set_volume(state.player.volume().min(output));
    if let Some(ramp) = state.volume_ramp {
        // Keep pause-fade completion and its deadline when boost is disabled.
        start_volume_ramp_with_completion(
            state,
            ramp.target_volume.min(output),
            ramp.duration.saturating_sub(ramp.started_at.elapsed()),
            ramp.completion,
        );
    }
}

fn steady_output_volume(state: &DaemonState) -> f64 {
    let base = if state.ducked {
        pipewire_duck_output_volume(state)
    } else {
        logical_volume(state)
    };
    (base * state.desktop_volume.comp_gain()).clamp(0.0, 5.0)
}

/// Desktop aware volume covers every coordination-driven interruption:
/// PipeWire duck, PipeWire pause, and MPRIS peer pause (halt and
/// halt-auto, including the pending auto-resume window).
fn desktop_volume_inputs(state: &DaemonState, now: Instant) -> DesktopVolumeInputs {
    let mpris_suppressed =
        state.peer_mpris.is_waiting_for_peer_pause() || mpris_halt_auto_resume_is_pending(state);
    let suppressed = state.ducked || state.pipewire_pause_active || mpris_suppressed;
    let mpris_foreign_active = state.peer_mpris.has_playing_services();
    let foreign_active = state.pipewire_trigger_active || mpris_foreign_active;
    let foreign_since = match (
        state.pipewire_trigger_since,
        state.peer_mpris.playing_services_since(),
    ) {
        (Some(pipewire), Some(mpris)) => Some(pipewire.min(mpris)),
        (pipewire, mpris) => pipewire.or(mpris),
    };
    DesktopVolumeInputs {
        now,
        enabled: state.settings.desktop_aware_volume
            && (state.settings.pipewire_ducking_enabled
                || state.settings.other_mpris_mode.coordinates_peers()),
        foreign_active,
        suppressed,
        playing_alone: state.active_track.is_some()
            && !suppressed
            && !foreign_active
            && !is_logically_paused(state),
        foreign_since,
        hold: Duration::from_secs(state.settings.desktop_aware_volume_hold_secs.into()),
        restore: Duration::from_secs(state.settings.desktop_aware_volume_restore_secs.into()),
    }
}

fn tick_desktop_aware_volume(state: &mut DaemonState) {
    let now = Instant::now();
    let inputs = desktop_volume_inputs(state, now);
    let comp_before = state.desktop_volume.comp_gain();
    state.desktop_volume.tick(&inputs);
    if !volumes_match(comp_before, state.desktop_volume.comp_gain()) {
        apply_desktop_volume_comp(state);
    }

    if state.desktop_volume_io_in_flight {
        return;
    }
    let pending_write = state.desktop_volume.take_pending_write();
    let due = state
        .last_desktop_volume_sample_at
        .is_none_or(|last| last.elapsed() >= DESKTOP_VOLUME_SAMPLE_INTERVAL);
    if pending_write.is_none() && !(due && state.desktop_volume.wants_samples(&inputs)) {
        return;
    }
    state.desktop_volume_io_in_flight = true;
    state.last_desktop_volume_sample_at = Some(now);
    let background_tx = state.background_tx.clone();
    std::thread::spawn(move || {
        if let Some((node_id, volume)) = pending_write {
            if !desktop_volume::write_sink_volume(node_id, volume) {
                eprintln!("[DESKTOP-VOLUME] failed to set sink {node_id} volume to {volume:.2}");
            }
        }
        let sample = desktop_volume::read_default_sink();
        let _ = background_tx.send(BackgroundEvent::SinkSampleReady { sample });
    });
}

/// Re-aim the output at the new compensated steady level without disturbing
/// a pause fade or a transport that is not audible anyway.
fn apply_desktop_volume_comp(state: &mut DaemonState) {
    if state.active_track.is_none() || is_logically_paused(state) {
        return;
    }
    let target = steady_output_volume(state);
    match state.volume_ramp {
        Some(ramp) if matches!(ramp.completion, VolumeRampCompletion::None) => {
            if !volumes_match(ramp.target_volume, target) {
                start_volume_ramp_with_completion(
                    state,
                    target,
                    ramp.duration.saturating_sub(ramp.started_at.elapsed()),
                    VolumeRampCompletion::None,
                );
            }
        }
        Some(_) => {}
        None => state.player.set_volume(target),
    }
}

fn desktop_volume_snapshot(state: &DaemonState) -> Option<crate::ipc::DesktopAwareVolumeSnapshot> {
    let status = state.desktop_volume.status()?;
    let inputs = desktop_volume_inputs(state, Instant::now());
    Some(crate::ipc::DesktopAwareVolumeSnapshot {
        phase: status.phase.as_str().to_string(),
        comp_db: status.comp_db,
        sink_delta_db: status.sink_delta_db,
        remaining_secs: state
            .desktop_volume
            .remaining(&inputs)
            .map(|remaining| remaining.as_secs()),
    })
}

fn pipewire_duck_output_volume(state: &DaemonState) -> f64 {
    (state.pre_duck_volume
        * state
            .settings
            .pipewire_ducking_volume
            .clamp(0.0, PIPEWIRE_DUCKING_VOLUME_CLI_MAX))
    .clamp(0.0, 5.0)
}

fn set_logical_volume(state: &mut DaemonState, volume: f64) {
    if !volume.is_finite() {
        return;
    }
    let volume = volume.clamp(0.0, state.settings.max_volume());
    state.pre_duck_volume = volume;

    if state.ducked {
        start_volume_ramp(state, steady_output_volume(state));
    } else {
        state.volume_ramp = None;
        state.player.set_volume(steady_output_volume(state));
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
    let target_volume = target_volume.clamp(0.0, 5.0);
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
    state.player.set_volume(final_volume.clamp(0.0, 5.0));
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
    if ramp.duration.is_zero() {
        return ramp.target_volume.clamp(0.0, 5.0);
    }
    let progress =
        (ramp.started_at.elapsed().as_secs_f64() / ramp.duration.as_secs_f64()).clamp(0.0, 1.0);
    (ramp.start_volume + (ramp.target_volume - ramp.start_volume) * progress).clamp(0.0, 5.0)
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

    let trigger_since = active_streams
        .values()
        .filter(|stream| {
            coordination_filter_allows(
                state.settings.pipewire_filter_mode,
                &state.settings.pipewire_filter_list,
                &stream.identifiers(),
            )
        })
        .filter(|stream| stream.first_seen.elapsed() >= threshold)
        .map(|stream| stream.first_seen)
        .min();
    state.pipewire_trigger_active = trigger_since.is_some();
    state.pipewire_trigger_since = trigger_since;
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
            start_volume_ramp(state, steady_output_volume(state));
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
    let (video_id, title, artist, thumb_url) = if let Some(track) = state.active_track.as_ref() {
        (
            track.video_id.clone(),
            track.title.clone(),
            track.artist.clone(),
            track.thumb_url.clone(),
        )
    } else if let Some(track) = pending_playback_track(state) {
        (
            track.video_id.clone(),
            track.title.clone(),
            artist_string(track),
            track.thumbnail_url.clone(),
        )
    } else {
        return Metadata::builder().build();
    };

    let mut builder = Metadata::builder()
        .trackid(mpris_track_id(&video_id))
        .title(title)
        .artist(vec![artist]);

    if let Some(duration_millis) = mpris_duration_millis(state) {
        builder = builder.length(MprisTime::from_micros((duration_millis * 1_000) as i64));
    }

    let art_url = state
        .cache
        .get_cached_art(&video_id)
        .and_then(|path| {
            url::Url::from_file_path(path)
                .ok()
                .map(|url| url.to_string())
        })
        .or(thumb_url);
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
