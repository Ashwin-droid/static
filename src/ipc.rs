use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::cartridges::{CHUNK_BASE64_MAX, CHUNK_BYTES, CartridgeInfo};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{Connection as SqliteConnection, OptionalExtension};
use serde::{Deserialize, Serialize};
use zbus::blocking::{Connection, Proxy};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use crate::logging::LogSource;
use crate::models::{RadioVirtualPlaylist, SyncedLyrics, Track};
use crate::network::SongRating;

pub const DAEMON_BUS_NAME: &str = "com.static.music.staticd";
pub const DAEMON_OBJECT_PATH: &str = "/com/static/music/Staticd";
pub const DAEMON_INTERFACE: &str = "com.static.music.Staticd1";
const PLAYBACK_STATE_DB_FILE_NAME: &str = "data.db";
const PLAYBACK_STATE_TABLE: &str = "playback_state";
const PLAYBACK_STATE_ROW_KEY: &str = "resume";
const PLAYLIST_STATE_ROW_KEY_PREFIX: &str = "playlist";
const PLAYBACK_STATE_DB_BUSY_TIMEOUT: Duration = Duration::from_secs(1);

pub fn youtube_music_watch_url(video_id: &str) -> String {
    format!("https://music.youtube.com/watch?v={video_id}")
}

/// Settings edited together by the dGain calibration window.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GainCalibrationSettings {
    pub enabled: bool,
    pub flatten_percent: f64,
    /// WRA interval over which lazyness-dependent input/history weights apply.
    pub ra_seconds: f64,
    pub change_lazyness_percent: f64,
    pub xdgain_enabled: bool,
}
impl Default for GainCalibrationSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            flatten_percent: 50.0,
            ra_seconds: 15.0,
            change_lazyness_percent: 50.0,
            xdgain_enabled: false,
        }
    }
}
impl GainCalibrationSettings {
    pub fn apply_to(&self, settings: &mut crate::offline::AppSettings) -> Result<()> {
        // Validate on a clone so a refused edit cannot partially mutate settings.
        let mut next = settings.clone();
        for (key, value) in [
            ("xdgain_enabled", "false".to_owned()),
            ("dgain_enabled", self.enabled.to_string()),
            ("dgain_flatten_percent", self.flatten_percent.to_string()),
            ("dgain_ra_seconds", self.ra_seconds.to_string()),
            (
                "dgain_change_lazyness",
                self.change_lazyness_percent.to_string(),
            ),
            ("xdgain_enabled", self.xdgain_enabled.to_string()),
        ] {
            crate::cli::apply_config_value(&mut next, key, &value)?;
        }
        *settings = next;
        Ok(())
    }
}

/// Measurements of one rendered PCM block, before the device/mixer volume.
/// RMS values are linear amplitudes; gain ratios use 1.0 for unity.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GainDiagnostics {
    pub playback_paused: bool,
    pub track: Option<ActiveTrackSnapshot>,
    pub generation: u64,
    pub sequence: u64,
    pub position_millis: Option<u64>,
    pub opening_guard_millis: u64,
    pub settings: GainCalibrationSettings,
    pub input_rms: f64,
    pub input_peak: f64,
    /// Correction against WRA (including the xdGain reference), before master/limiter.
    pub gain_output_rms: f64,
    pub gain_output_peak: f64,
    pub output_rms: f64,
    pub output_peak: f64,
    /// WRA blended in dBFS, converted to linear amplitude for level metering.
    pub rolling_rms: f64,
    /// Audio observed since the WRA reset; there is no finite expiry window.
    pub window_seconds: f64,
    pub audible_seconds: f64,
    pub requested_gain: f64,
    pub preview_gain: f64,
    pub applied_gain: f64,
    /// Fixed last emitted WRA of the previous song; absent on the first song.
    pub xdgain_reference_dbfs: Option<f64>,
    pub limiter_gain: f64,
    pub master_gain: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepeatMode {
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn next(self, _is_radio: bool) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveTrackSnapshot {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub thumb_url: Option<String>,
    pub rating: SongRating,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackSnapshot {
    pub current_track: Option<ActiveTrackSnapshot>,
    pub queue: Vec<Track>,
    pub queue_index: usize,
    pub playlist_context: Option<PlaylistContext>,
    pub radio_virtual_playlist: Option<RadioVirtualPlaylist>,
    pub repeat_mode: RepeatMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_repeat_loops_left: Option<u32>,
    #[serde(default)]
    pub shuffle: bool,
    /// Pre-decided radio-shuffle pick advertised as a "Next up" hint (populated
    /// only while shuffle + radio are active and repeat is Off — repeat-One and
    /// repeat-All prefix loops never reach the untouched region). UI hint only:
    /// MPRIS metadata and the queue order stay untouched until the advance
    /// actually happens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up_next: Option<Track>,
    pub paused: bool,
    pub volume: f64,
    /// Current user volume limit, owned by the daemon's playback settings.
    #[serde(default = "default_max_volume")]
    pub max_volume: f64,
    pub position_millis: Option<u64>,
    pub duration_millis: Option<u64>,
    pub buffered_fraction: f64,
    #[serde(default)]
    pub halt_auto_resume_pending: bool,
    #[serde(default)]
    pub probe_active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_restore_position_millis: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleep_remaining_secs: Option<u64>,
    #[serde(default)]
    pub sleep_after_current: bool,
    #[serde(default)]
    pub ducked: bool,
    #[serde(default)]
    pub pipewire_pause_active: bool,
    /// Dynamic PipeWire duck in effect (None when not ducked).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duck: Option<DuckSnapshot>,
    /// Desktop aware volume job in flight (None when idle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_aware_volume: Option<DesktopAwareVolumeSnapshot>,
    /// A staticfs unmount is blocked by open handles and awaiting a
    /// close-anyway/wait decision (staticfs builds only; pending halt-auto
    /// pattern: GUI/TUI render controls from this, CLI/fs show nothing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staticfs_unmount_pending: Option<StaticfsUnmountPending>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DuckSnapshot {
    /// 0 = untouched, 1 = silent.
    pub amount: f64,
    /// Smoothed combined level of the tapped foreign streams, in dBFS.
    pub level_dbfs: f64,
    /// Whether at least one foreign stream is being tapped right now.
    pub tapped: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DesktopAwareVolumeSnapshot {
    /// interrupted | resuming | hold | restore | fade_out
    pub phase: String,
    /// Compensation currently applied to Static's output, in dB.
    pub comp_db: f64,
    /// How far the sink sits from its pre-interruption level, in dB.
    pub sink_delta_db: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StaticfsUnmountPending {
    /// Mount-relative paths of the busy files (or the mountpoint itself when
    /// something like a shell cwd pins the mount without an open handle).
    pub busy_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoordinationTargetSnapshot {
    pub key: String,
    pub label: String,
    pub detail: String,
    pub active: bool,
    #[serde(default = "default_coordination_target_filterable")]
    pub filterable: bool,
}

fn default_coordination_target_filterable() -> bool {
    true
}

fn default_max_volume() -> f64 {
    1.0
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoordinationTargetsSnapshot {
    pub mpris: Vec<CoordinationTargetSnapshot>,
    pub pipewire: Vec<CoordinationTargetSnapshot>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SleepStatusSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_secs: Option<u64>,
    #[serde(default)]
    pub after_current: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedPlaybackState {
    pub snapshot: PlaybackSnapshot,
    pub saved_at_unix_millis: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaylistContextSource {
    Remote,
    Offline,
    Browse,
}

impl PlaylistContextSource {
    fn storage_key_segment(self) -> &'static str {
        match self {
            Self::Remote => "remote",
            Self::Offline => "offline",
            Self::Browse => "browse",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistContext {
    pub playlist_id: String,
    pub title: String,
    pub thumbnail_url: Option<String>,
    pub subtitle: Option<String>,
    pub source: PlaylistContextSource,
}

impl PlaylistContext {
    fn storage_key(&self) -> String {
        format!(
            "{PLAYLIST_STATE_ROW_KEY_PREFIX}:{}:{}",
            self.source.storage_key_segment(),
            self.playlist_id
        )
    }
}

impl Default for PlaybackSnapshot {
    fn default() -> Self {
        Self {
            current_track: None,
            queue: Vec::new(),
            queue_index: 0,
            playlist_context: None,
            radio_virtual_playlist: None,
            repeat_mode: RepeatMode::Off,
            track_repeat_loops_left: None,
            shuffle: false,
            up_next: None,
            paused: false,
            volume: 1.0,
            max_volume: 1.0,
            position_millis: None,
            duration_millis: None,
            buffered_fraction: 0.0,
            halt_auto_resume_pending: false,
            probe_active: false,
            probe_restore_position_millis: None,
            sleep_remaining_secs: None,
            sleep_after_current: false,
            ducked: false,
            pipewire_pause_active: false,
            duck: None,
            desktop_aware_volume: None,
            staticfs_unmount_pending: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualizationFrame {
    pub generation: u64,
    pub sequence: u64,
    pub sample_rate: u32,
    pub bands: Vec<f32>,
    pub rms: f32,
    pub peak: f32,
}

/// Audio source for a native cartridge worker; never sent to the web page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualizationSource {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub uri: String,
    pub storage_root: PathBuf,
    pub staticfs_enabled: bool,
    #[serde(default)]
    pub staticfs_mountpoint: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum DaemonRequest {
    BeginCartridgeUpload {
        name: String,
    },
    AppendCartridgeUpload {
        id: String,
        data_base64: String,
    },
    CommitCartridgeUpload {
        id: String,
    },
    AbortCartridgeUpload {
        id: String,
    },
    ListMemoryCartridges,
    MemoryCartridgeInfo {
        name: String,
    },
    ReadMemoryCartridge {
        name: String,
        offset: u64,
        size: u32,
    },
    RemoveMemoryCartridge {
        name: String,
    },
    Ping,
    GetState,
    GetGainDiagnostics,
    SetGainCalibration {
        settings: GainCalibrationSettings,
    },
    GetVisualizationFrame {
        bands: u32,
    },
    GetVisualizationSource {
        video_id: String,
        /// Older visualizer processes must reopen before exporting in memory mode.
        #[serde(default)]
        supports_memory: bool,
    },
    RateCurrentTrack {
        rating: SongRating,
    },
    PlayQueueAt {
        queue: Vec<Track>,
        index: usize,
        playlist_context: Option<PlaylistContext>,
        radio_virtual_playlist: Option<RadioVirtualPlaylist>,
        repeat_mode: RepeatMode,
        #[serde(default = "default_true")]
        pause_other_mpris_players: bool,
        #[serde(default)]
        start_paused: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start_position_millis: Option<u64>,
        #[serde(default)]
        suppress_reporting: bool,
    },
    ReplaceQueue {
        queue: Vec<Track>,
        index: usize,
        playlist_context: Option<PlaylistContext>,
        radio_virtual_playlist: Option<RadioVirtualPlaylist>,
        repeat_mode: RepeatMode,
    },
    StartRadioFromSeed {
        queue: Vec<Track>,
        index: usize,
        seed_index: usize,
        prefix_len: usize,
        playlist_context: Option<PlaylistContext>,
        repeat_mode: RepeatMode,
        play: bool,
        #[serde(default)]
        suppress_reporting: bool,
    },
    InsertIntoQueue {
        tracks: Vec<Track>,
        index: usize,
    },
    RemoveFromQueue {
        index: usize,
    },
    MoveQueueItem {
        from: usize,
        raw_slot: usize,
    },
    DismissQueueAfterCurrent,
    ProbeTrack {
        track: Track,
    },
    TogglePause,
    Pause,
    Resume,
    CancelHaltAutoResume,
    ResumeHaltAutoResume,
    Stop,
    Next,
    Previous,
    SeekToMillis {
        position_millis: u64,
    },
    SetVolume {
        volume: f64,
    },
    SetRepeatMode {
        repeat_mode: RepeatMode,
    },
    SetShuffle {
        enabled: bool,
    },
    ActivateRadioChip {
        chip_index: usize,
    },
    SetSleepTimer {
        minutes: u32,
    },
    SetSleepAfterCurrent,
    ClearSleepTimer,
    GetSleepStatus,
    RegisterUiClient {
        client_id: String,
    },
    UnregisterUiClient {
        client_id: String,
    },
    CopyShareUrl,
    LogMessage {
        source: LogSource,
        message: String,
    },
    GetCoordinationTargets,
    ReloadSettings,
    GetSettings,
    SetSetting {
        key: String,
        value: String,
    },
    ResolveAndQueue {
        query: String,
        /// false = CLI `queue addLast` slot (touched boundary), true =
        /// CLI `queue addNext` slot (right after the current track).
        #[serde(default)]
        next: bool,
    },
    ResolveAndPlayRadio {
        query: String,
        /// true = always resolve via search, even when the query looks like
        /// a bare video id (explicit "this is a song name" intent, e.g. the
        /// staticfs `playback/video_name` write).
        #[serde(default)]
        force_search: bool,
    },
    GetLyrics,
    ClearQueue,
    Shutdown,
    /// "Close anyway" on a pending staticfs unmount: lazy-detach now.
    StaticfsUnmountNow,
    /// "Wait" on a pending staticfs unmount: dismiss the prompt and unmount
    /// once the last open handle closes.
    StaticfsUnmountWait,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum DaemonResponse {
    CartridgeUpload(String),
    MemoryCartridges(Vec<CartridgeInfo>),
    MemoryCartridgeInfo(Option<CartridgeInfo>),
    CartridgeCommitted(CartridgeInfo),
    CartridgeChunk(Option<String>),
    CartridgeRemoved(bool),
    Ok,
    Pong,
    State(PlaybackSnapshot),
    GainDiagnostics(GainDiagnostics),
    VisualizationFrame(VisualizationFrame),
    VisualizationSource(VisualizationSource),
    CoordinationTargets(CoordinationTargetsSnapshot),
    SleepStatus(SleepStatusSnapshot),
    ShareUrl(String),
    Settings(Vec<(String, String)>),
    Lyrics(Option<SyncedLyrics>),
    Error(String),
    /// The daemon's D-Bus handler gave up waiting for the main loop
    /// (envelope timeout/disconnect): the request was NOT rejected — it may
    /// even still execute once the loop unblocks — so clients must treat
    /// this as a transport-level failure (staticfs: EIO), never as a
    /// daemon-side validation error (`Error` -> EINVAL). Mapped to an `Err`
    /// in `send_request_raw_with_connection`, so no response matcher ever
    /// sees it.
    Unavailable(String),
}

/// Error type for `DaemonClient::try_*` methods, distinguishing a request
/// the daemon received and rejected (validation failure; FUSE maps this to
/// EINVAL) from a request that never completed (transport failure or a
/// malformed/unexpected reply; FUSE maps this to EIO).
// Consumed by the staticfs FUSE layer only, so dead in feature-off builds.
#[cfg_attr(not(feature = "staticfs"), allow(dead_code))]
#[derive(Debug)]
pub enum DaemonCallError {
    /// The daemon handled the request and answered `DaemonResponse::Error`.
    Rejected(String),
    /// The daemon could not be reached, or the reply could not be understood.
    Transport(anyhow::Error),
}

impl std::fmt::Display for DaemonCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(message) => write!(f, "{message}"),
            Self::Transport(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for DaemonCallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rejected(_) => None,
            Self::Transport(err) => Some(err.as_ref()),
        }
    }
}

fn daemon_connection() -> Result<Connection> {
    Connection::session().context("Failed to connect to the session D-Bus")
}

fn daemon_proxy(connection: &Connection) -> Result<Proxy<'_>> {
    Proxy::new(
        connection,
        DAEMON_BUS_NAME,
        DAEMON_OBJECT_PATH,
        DAEMON_INTERFACE,
    )
    .context("Failed to create staticd D-Bus proxy")
}

fn send_request_raw_with_connection(
    connection: &Connection,
    request: &DaemonRequest,
) -> Result<DaemonResponse> {
    let proxy = daemon_proxy(connection)?;
    let request_json = serde_json::to_string(request)?;
    let response_json: String = proxy
        .call("RequestJson", &(request_json.as_str(),))
        .context("staticd D-Bus request failed")?;
    match serde_json::from_str(&response_json)? {
        // The envelope reply timed out or its channel dropped: the daemon
        // never answered (though the queued command may still run later).
        // Surface it as a transport failure so `try_request` classifies it
        // as `DaemonCallError::Transport` (staticfs: EIO, not EINVAL) and
        // the plain methods bail with the message.
        DaemonResponse::Unavailable(message) => bail!(message),
        response => Ok(response),
    }
}

fn send_request_raw(request: &DaemonRequest) -> Result<DaemonResponse> {
    let connection = daemon_connection()?;
    send_request_raw_with_connection(&connection, request)
}

fn playback_state_database_path() -> PathBuf {
    crate::logging::static_root().join(PLAYBACK_STATE_DB_FILE_NAME)
}

fn open_playback_state_database(path: &Path) -> Result<SqliteConnection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create playback state dir {}", parent.display()))?;
    }

    let db = SqliteConnection::open(path).with_context(|| {
        format!(
            "Failed to open playback state database at {}",
            path.display()
        )
    })?;
    db.busy_timeout(PLAYBACK_STATE_DB_BUSY_TIMEOUT)
        .with_context(|| {
            format!(
                "Failed to configure playback state database at {}",
                path.display()
            )
        })?;
    db.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {PLAYBACK_STATE_TABLE} (
            key     TEXT PRIMARY KEY,
            payload TEXT NOT NULL
        );"
    ))
    .with_context(|| {
        format!(
            "Failed to initialize playback state database at {}",
            path.display()
        )
    })?;

    Ok(db)
}

fn load_persisted_state_at_path(path: &Path, key: &str) -> Result<Option<PersistedPlaybackState>> {
    if !path.exists() {
        return Ok(None);
    }

    let db = open_playback_state_database(path)?;
    let payload = db
        .query_row(
            &format!("SELECT payload FROM {PLAYBACK_STATE_TABLE} WHERE key = ?1"),
            [key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .with_context(|| {
            format!(
                "Failed to read persisted playback state from {}",
                path.display()
            )
        })?;

    let Some(payload) = payload else {
        return Ok(None);
    };

    serde_json::from_str(&payload)
        .with_context(|| {
            format!(
                "Failed to parse persisted playback state from {}",
                path.display()
            )
        })
        .map(Some)
}

fn store_persisted_state_at_path(
    path: &Path,
    key: &str,
    state: &PersistedPlaybackState,
) -> Result<()> {
    let db = open_playback_state_database(path)?;
    let payload = serde_json::to_string(state)?;
    db.execute(
        &format!(
            "INSERT INTO {PLAYBACK_STATE_TABLE} (key, payload) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET payload = excluded.payload"
        ),
        (key, payload),
    )
    .with_context(|| format!("Failed to persist playback state in {}", path.display()))?;
    Ok(())
}

fn clear_persisted_state_at_path(path: &Path, key: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let db = open_playback_state_database(path)?;
    db.execute(
        &format!("DELETE FROM {PLAYBACK_STATE_TABLE} WHERE key = ?1"),
        [key],
    )
    .with_context(|| {
        format!(
            "Failed to clear persisted playback state from {}",
            path.display()
        )
    })?;
    Ok(())
}

pub fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn load_persisted_playback_state() -> Result<Option<PersistedPlaybackState>> {
    load_persisted_state_at_path(&playback_state_database_path(), PLAYBACK_STATE_ROW_KEY)
}

pub fn store_persisted_playback_state(state: &PersistedPlaybackState) -> Result<()> {
    store_persisted_state_at_path(
        &playback_state_database_path(),
        PLAYBACK_STATE_ROW_KEY,
        state,
    )
}

pub fn clear_persisted_playback_state() -> Result<()> {
    clear_persisted_state_at_path(&playback_state_database_path(), PLAYBACK_STATE_ROW_KEY)
}

pub fn load_persisted_playlist_state(
    playlist_context: &PlaylistContext,
) -> Result<Option<PersistedPlaybackState>> {
    load_persisted_state_at_path(
        &playback_state_database_path(),
        &playlist_context.storage_key(),
    )
}

pub fn store_persisted_playlist_state(
    playlist_context: &PlaylistContext,
    state: &PersistedPlaybackState,
) -> Result<()> {
    store_persisted_state_at_path(
        &playback_state_database_path(),
        &playlist_context.storage_key(),
        state,
    )
}

pub fn ensure_daemon_running() -> Result<()> {
    if matches!(
        send_request_raw(&DaemonRequest::Ping),
        Ok(DaemonResponse::Pong)
    ) {
        return Ok(());
    }

    let exe = std::env::current_exe().context("Failed to locate current executable")?;
    let mut command = Command::new(exe);
    command
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }

    command.spawn().context("Failed to spawn playback daemon")?;

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        match send_request_raw(&DaemonRequest::Ping) {
            Ok(DaemonResponse::Pong) => return Ok(()),
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }

    bail!("Playback daemon did not become ready in time")
}

pub fn daemon_is_running() -> bool {
    matches!(
        send_request_raw(&DaemonRequest::Ping),
        Ok(DaemonResponse::Pong)
    )
}

pub fn forward_log_message(source: LogSource, message: String) -> Result<()> {
    match send_request_raw(&DaemonRequest::LogMessage { source, message })? {
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::Error(err) => bail!(err),
        other => bail!("Unexpected daemon response: {other:?}"),
    }
}

#[derive(Clone)]
pub struct DaemonClient {
    connection: Connection,
}

impl DaemonClient {
    pub fn begin_cartridge_upload(&self, name: &str) -> Result<String> {
        match self.request(&DaemonRequest::BeginCartridgeUpload {
            name: name.to_owned(),
        })? {
            DaemonResponse::CartridgeUpload(id) => Ok(id),
            DaemonResponse::Error(error) => bail!(error),
            other => bail!("Unexpected cartridge upload response: {other:?}"),
        }
    }

    pub fn append_cartridge_upload(&self, id: &str, bytes: &[u8]) -> Result<()> {
        if bytes.len() > CHUNK_BYTES {
            bail!("Cartridge chunk is too large");
        }
        match self.request(&DaemonRequest::AppendCartridgeUpload {
            id: id.to_owned(),
            data_base64: STANDARD.encode(bytes),
        })? {
            DaemonResponse::Ok => Ok(()),
            DaemonResponse::Error(error) => bail!(error),
            other => bail!("Unexpected cartridge upload response: {other:?}"),
        }
    }

    pub fn commit_cartridge_upload(&self, id: &str) -> Result<CartridgeInfo> {
        match self.request(&DaemonRequest::CommitCartridgeUpload { id: id.to_owned() })? {
            DaemonResponse::CartridgeCommitted(info) => Ok(info),
            DaemonResponse::Error(error) => bail!(error),
            other => bail!("Unexpected cartridge commit response: {other:?}"),
        }
    }

    pub fn abort_cartridge_upload(&self, id: &str) -> Result<()> {
        match self.request(&DaemonRequest::AbortCartridgeUpload { id: id.to_owned() })? {
            DaemonResponse::Ok => Ok(()),
            DaemonResponse::Error(error) => bail!(error),
            other => bail!("Unexpected cartridge abort response: {other:?}"),
        }
    }

    #[cfg_attr(not(feature = "staticfs"), allow(dead_code))]
    pub fn try_list_memory_cartridges(
        &self,
    ) -> std::result::Result<Vec<CartridgeInfo>, DaemonCallError> {
        match self.try_request(&DaemonRequest::ListMemoryCartridges)? {
            DaemonResponse::MemoryCartridges(entries) => Ok(entries),
            other => Err(DaemonCallError::Transport(anyhow::anyhow!(
                "Unexpected cartridge list response: {other:?}"
            ))),
        }
    }

    pub fn try_memory_cartridge_info(
        &self,
        name: &str,
    ) -> std::result::Result<Option<CartridgeInfo>, DaemonCallError> {
        match self.try_request(&DaemonRequest::MemoryCartridgeInfo {
            name: name.to_owned(),
        })? {
            DaemonResponse::MemoryCartridgeInfo(info) => Ok(info),
            other => Err(DaemonCallError::Transport(anyhow::anyhow!(
                "Unexpected cartridge info response: {other:?}"
            ))),
        }
    }

    pub fn try_read_memory_cartridge(
        &self,
        name: &str,
        offset: u64,
        size: u32,
    ) -> std::result::Result<Option<Vec<u8>>, DaemonCallError> {
        match self.try_request(&DaemonRequest::ReadMemoryCartridge {
            name: name.to_owned(),
            offset,
            size: size.min(CHUNK_BYTES as u32),
        })? {
            DaemonResponse::CartridgeChunk(Some(encoded)) => {
                if encoded.len() > CHUNK_BASE64_MAX {
                    return Err(DaemonCallError::Transport(anyhow::anyhow!(
                        "Cartridge reply exceeds chunk limit"
                    )));
                }
                let bytes = STANDARD
                    .decode(encoded)
                    .map_err(|error| DaemonCallError::Transport(error.into()))?;
                if bytes.len() > (size as usize).min(CHUNK_BYTES) {
                    return Err(DaemonCallError::Transport(anyhow::anyhow!(
                        "Cartridge reply exceeds requested length"
                    )));
                }
                Ok(Some(bytes))
            }
            DaemonResponse::CartridgeChunk(None) => Ok(None),
            other => Err(DaemonCallError::Transport(anyhow::anyhow!(
                "Unexpected cartridge read response: {other:?}"
            ))),
        }
    }

    #[cfg_attr(not(feature = "staticfs"), allow(dead_code))]
    pub fn try_remove_memory_cartridge(
        &self,
        name: &str,
    ) -> std::result::Result<bool, DaemonCallError> {
        match self.try_request(&DaemonRequest::RemoveMemoryCartridge {
            name: name.to_owned(),
        })? {
            DaemonResponse::CartridgeRemoved(removed) => Ok(removed),
            other => Err(DaemonCallError::Transport(anyhow::anyhow!(
                "Unexpected cartridge remove response: {other:?}"
            ))),
        }
    }

    pub fn gain_diagnostics(&self) -> Result<GainDiagnostics> {
        match self.request(&DaemonRequest::GetGainDiagnostics)? {
            DaemonResponse::GainDiagnostics(frame) => Ok(frame),
            DaemonResponse::Error(error) => bail!(error),
            other => bail!("Unexpected dGain diagnostics response: {other:?}"),
        }
    }

    pub fn set_gain_calibration(
        &self,
        settings: GainCalibrationSettings,
    ) -> Result<PlaybackSnapshot> {
        Ok(self.try_state_request(&DaemonRequest::SetGainCalibration { settings })?)
    }

    pub fn visualization_frame(&self, bands: u32) -> Result<VisualizationFrame> {
        match self.request(&DaemonRequest::GetVisualizationFrame { bands })? {
            DaemonResponse::VisualizationFrame(frame) => Ok(frame),
            DaemonResponse::Error(err) => bail!(err),
            other => bail!("Unexpected visualization response: {other:?}"),
        }
    }

    pub fn visualization_source(&self, video_id: &str) -> Result<VisualizationSource> {
        match self.request(&DaemonRequest::GetVisualizationSource {
            video_id: video_id.to_owned(),
            supports_memory: true,
        })? {
            DaemonResponse::VisualizationSource(source) => Ok(source),
            DaemonResponse::Error(err) => bail!(err),
            other => bail!("Unexpected visualization source response: {other:?}"),
        }
    }

    pub fn new() -> Result<Self> {
        ensure_daemon_running()?;
        Ok(Self {
            connection: daemon_connection()?,
        })
    }

    fn request(&self, request: &DaemonRequest) -> Result<DaemonResponse> {
        send_request_raw_with_connection(&self.connection, request)
    }

    pub fn state(&self) -> Result<PlaybackSnapshot> {
        match self.request(&DaemonRequest::GetState)? {
            DaemonResponse::State(state) => Ok(state),
            DaemonResponse::Error(err) => bail!(err),
            other => bail!("Unexpected daemon response: {other:?}"),
        }
    }

    pub fn rate_current_track(&self, rating: SongRating) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::RateCurrentTrack { rating })
    }

    pub fn play_queue_at(
        &self,
        queue: Vec<Track>,
        index: usize,
        playlist_context: Option<PlaylistContext>,
        radio_virtual_playlist: Option<RadioVirtualPlaylist>,
        repeat_mode: RepeatMode,
    ) -> Result<PlaybackSnapshot> {
        self.play_queue_at_with_options(
            queue,
            index,
            playlist_context,
            radio_virtual_playlist,
            repeat_mode,
            true,
            false,
            None,
            false,
        )
    }

    pub fn restore_queue_at(
        &self,
        queue: Vec<Track>,
        index: usize,
        playlist_context: Option<PlaylistContext>,
        radio_virtual_playlist: Option<RadioVirtualPlaylist>,
        repeat_mode: RepeatMode,
        pause_other_mpris_players: bool,
        start_paused: bool,
        start_position_millis: Option<u64>,
    ) -> Result<PlaybackSnapshot> {
        self.play_queue_at_with_options(
            queue,
            index,
            playlist_context,
            radio_virtual_playlist,
            repeat_mode,
            pause_other_mpris_players,
            start_paused,
            start_position_millis,
            false,
        )
    }

    fn play_queue_at_with_options(
        &self,
        queue: Vec<Track>,
        index: usize,
        playlist_context: Option<PlaylistContext>,
        radio_virtual_playlist: Option<RadioVirtualPlaylist>,
        repeat_mode: RepeatMode,
        pause_other_mpris_players: bool,
        start_paused: bool,
        start_position_millis: Option<u64>,
        suppress_reporting: bool,
    ) -> Result<PlaybackSnapshot> {
        match self.request(&DaemonRequest::PlayQueueAt {
            queue,
            index,
            playlist_context,
            radio_virtual_playlist,
            repeat_mode,
            pause_other_mpris_players,
            start_paused,
            start_position_millis,
            suppress_reporting,
        })? {
            DaemonResponse::State(state) => Ok(state),
            DaemonResponse::Error(err) => bail!(err),
            other => bail!("Unexpected daemon response: {other:?}"),
        }
    }

    pub fn replace_queue(
        &self,
        queue: Vec<Track>,
        index: usize,
        playlist_context: Option<PlaylistContext>,
        radio_virtual_playlist: Option<RadioVirtualPlaylist>,
        repeat_mode: RepeatMode,
    ) -> Result<PlaybackSnapshot> {
        match self.request(&DaemonRequest::ReplaceQueue {
            queue,
            index,
            playlist_context,
            radio_virtual_playlist,
            repeat_mode,
        })? {
            DaemonResponse::State(state) => Ok(state),
            DaemonResponse::Error(err) => bail!(err),
            other => bail!("Unexpected daemon response: {other:?}"),
        }
    }

    pub fn start_radio_from_seed(
        &self,
        queue: Vec<Track>,
        index: usize,
        seed_index: usize,
        prefix_len: usize,
        playlist_context: Option<PlaylistContext>,
        repeat_mode: RepeatMode,
        play: bool,
    ) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::StartRadioFromSeed {
            queue,
            index,
            seed_index,
            prefix_len,
            playlist_context,
            repeat_mode,
            play,
            suppress_reporting: false,
        })
    }

    pub fn insert_into_queue(&self, tracks: Vec<Track>, index: usize) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::InsertIntoQueue { tracks, index })
    }

    pub fn remove_from_queue(&self, index: usize) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::RemoveFromQueue { index })
    }

    pub fn move_queue_item(&self, from: usize, raw_slot: usize) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::MoveQueueItem { from, raw_slot })
    }

    pub fn dismiss_queue_after_current(&self) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::DismissQueueAfterCurrent)
    }

    pub fn probe_track(&self, track: Track) -> Result<PlaybackSnapshot> {
        match self.request(&DaemonRequest::ProbeTrack { track })? {
            DaemonResponse::State(state) => Ok(state),
            DaemonResponse::Error(err) => bail!(err),
            other => bail!("Unexpected daemon response: {other:?}"),
        }
    }

    pub fn toggle_pause(&self) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::TogglePause)
    }

    pub fn pause(&self) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::Pause)
    }

    pub fn resume(&self) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::Resume)
    }

    pub fn cancel_halt_auto_resume(&self) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::CancelHaltAutoResume)
    }

    pub fn resume_halt_auto(&self) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::ResumeHaltAutoResume)
    }

    pub fn staticfs_unmount_now(&self) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::StaticfsUnmountNow)
    }

    pub fn staticfs_unmount_wait(&self) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::StaticfsUnmountWait)
    }

    pub fn stop(&self) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::Stop)
    }

    pub fn next(&self) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::Next)
    }

    pub fn previous(&self) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::Previous)
    }

    pub fn seek_to_millis(&self, position_millis: u64) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::SeekToMillis { position_millis })
    }

    pub fn set_volume(&self, volume: f64) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::SetVolume { volume })
    }

    pub fn set_repeat_mode(&self, repeat_mode: RepeatMode) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::SetRepeatMode { repeat_mode })
    }

    pub fn set_shuffle(&self, enabled: bool) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::SetShuffle { enabled })
    }

    pub fn activate_radio_chip(&self, chip_index: usize) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::ActivateRadioChip { chip_index })
    }

    pub fn set_sleep_timer(&self, minutes: u32) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::SetSleepTimer { minutes })
    }

    pub fn set_sleep_after_current(&self) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::SetSleepAfterCurrent)
    }

    pub fn clear_sleep_timer(&self) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::ClearSleepTimer)
    }

    pub fn sleep_status(&self) -> Result<SleepStatusSnapshot> {
        match self.request(&DaemonRequest::GetSleepStatus)? {
            DaemonResponse::SleepStatus(status) => Ok(status),
            DaemonResponse::Error(err) => bail!(err),
            other => bail!("Unexpected daemon response: {other:?}"),
        }
    }

    pub fn register_ui_client(&self, client_id: &str) -> Result<()> {
        match self.request(&DaemonRequest::RegisterUiClient {
            client_id: client_id.to_string(),
        })? {
            DaemonResponse::Ok => Ok(()),
            DaemonResponse::Error(err) => bail!(err),
            other => bail!("Unexpected daemon response: {other:?}"),
        }
    }

    pub fn unregister_ui_client(&self, client_id: &str) -> Result<()> {
        match self.request(&DaemonRequest::UnregisterUiClient {
            client_id: client_id.to_string(),
        })? {
            DaemonResponse::Ok => Ok(()),
            DaemonResponse::Error(err) => bail!(err),
            other => bail!("Unexpected daemon response: {other:?}"),
        }
    }

    pub fn copy_share_url(&self) -> Result<String> {
        match self.request(&DaemonRequest::CopyShareUrl)? {
            DaemonResponse::ShareUrl(url) => Ok(url),
            DaemonResponse::Error(err) => bail!(err),
            other => bail!("Unexpected daemon response: {other:?}"),
        }
    }

    pub fn reload_settings(&self) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::ReloadSettings)
    }

    pub fn coordination_targets(&self) -> Result<CoordinationTargetsSnapshot> {
        match self.request(&DaemonRequest::GetCoordinationTargets)? {
            DaemonResponse::CoordinationTargets(targets) => Ok(targets),
            DaemonResponse::Error(err) => bail!(err),
            other => bail!("Unexpected daemon response: {other:?}"),
        }
    }

    pub fn shutdown(&self) -> Result<()> {
        match self.request(&DaemonRequest::Shutdown)? {
            DaemonResponse::Ok => Ok(()),
            DaemonResponse::Error(err) => bail!(err),
            other => bail!("Unexpected daemon response: {other:?}"),
        }
    }

    fn transport_request(&self, request: DaemonRequest) -> Result<PlaybackSnapshot> {
        match self.request(&request)? {
            DaemonResponse::State(state) => Ok(state),
            DaemonResponse::Error(err) => bail!(err),
            other => bail!("Unexpected daemon response: {other:?}"),
        }
    }
}

/// Client surface for the staticfs FUSE layer: connection without daemon
/// auto-spawn, plus `try_*` methods that keep daemon-side rejections
/// (`DaemonCallError::Rejected` -> EINVAL) distinguishable from transport
/// failures (`DaemonCallError::Transport` -> EIO).
// Consumed by the staticfs FUSE layer only, so dead in feature-off builds.
#[cfg_attr(not(feature = "staticfs"), allow(dead_code))]
impl DaemonClient {
    /// Connect to an already-running daemon WITHOUT auto-spawning one
    /// (unlike `new()`). Pings the daemon to verify liveness and errors if
    /// it is not running.
    pub fn connect_existing() -> Result<Self> {
        let connection = daemon_connection()?;
        match send_request_raw_with_connection(&connection, &DaemonRequest::Ping) {
            Ok(DaemonResponse::Pong) => Ok(Self { connection }),
            Ok(other) => bail!("Unexpected daemon response to ping: {other:?}"),
            Err(err) => Err(err.context("Playback daemon is not running")),
        }
    }

    /// Send a request, splitting failures into `DaemonCallError::Rejected`
    /// (the daemon answered `DaemonResponse::Error`) and
    /// `DaemonCallError::Transport` (the request never completed). Never
    /// auto-spawns the daemon. Any non-error response is returned as-is.
    pub fn try_request(
        &self,
        request: &DaemonRequest,
    ) -> std::result::Result<DaemonResponse, DaemonCallError> {
        match self.request(request) {
            Ok(DaemonResponse::Error(message)) => Err(DaemonCallError::Rejected(message)),
            Ok(response) => Ok(response),
            Err(err) => Err(DaemonCallError::Transport(err)),
        }
    }

    /// Like `try_request`, for requests answered with `DaemonResponse::State`.
    /// An unexpected response variant counts as a transport failure.
    pub fn try_state_request(
        &self,
        request: &DaemonRequest,
    ) -> std::result::Result<PlaybackSnapshot, DaemonCallError> {
        match self.try_request(request)? {
            DaemonResponse::State(state) => Ok(state),
            other => Err(DaemonCallError::Transport(anyhow::anyhow!(
                "Unexpected daemon response: {other:?}"
            ))),
        }
    }

    /// `GetSettings` with the `Rejected`/`Transport` error split.
    pub fn try_settings(&self) -> std::result::Result<Vec<(String, String)>, DaemonCallError> {
        match self.try_request(&DaemonRequest::GetSettings)? {
            DaemonResponse::Settings(pairs) => Ok(pairs),
            other => Err(DaemonCallError::Transport(anyhow::anyhow!(
                "Unexpected daemon response: {other:?}"
            ))),
        }
    }

    /// `SetSetting` with the `Rejected`/`Transport` error split.
    pub fn try_set_setting(
        &self,
        key: &str,
        value: &str,
    ) -> std::result::Result<PlaybackSnapshot, DaemonCallError> {
        self.try_state_request(&DaemonRequest::SetSetting {
            key: key.to_string(),
            value: value.to_string(),
        })
    }

    /// `ResolveAndQueue` with the `Rejected`/`Transport` error split.
    /// `next: false` inserts at the CLI `queue addLast` slot (touched
    /// boundary), `next: true` at the `queue addNext` slot.
    pub fn try_resolve_and_queue(
        &self,
        query: &str,
        next: bool,
    ) -> std::result::Result<PlaybackSnapshot, DaemonCallError> {
        self.try_state_request(&DaemonRequest::ResolveAndQueue {
            query: query.to_string(),
            next,
        })
    }

    /// `ResolveAndPlayRadio` with the `Rejected`/`Transport` error split.
    /// `force_search: true` skips the bare-video-id interpretation and always
    /// resolves the query via search.
    pub fn try_resolve_and_play_radio(
        &self,
        query: &str,
        force_search: bool,
    ) -> std::result::Result<PlaybackSnapshot, DaemonCallError> {
        self.try_state_request(&DaemonRequest::ResolveAndPlayRadio {
            query: query.to_string(),
            force_search,
        })
    }

    /// `GetLyrics` with the `Rejected`/`Transport` error split. `Ok(None)`
    /// means the daemon looked and found no lyrics for the current track.
    pub fn try_lyrics(&self) -> std::result::Result<Option<SyncedLyrics>, DaemonCallError> {
        match self.try_request(&DaemonRequest::GetLyrics)? {
            DaemonResponse::Lyrics(lyrics) => Ok(lyrics),
            other => Err(DaemonCallError::Transport(anyhow::anyhow!(
                "Unexpected daemon response: {other:?}"
            ))),
        }
    }

    /// `ClearQueue` with the `Rejected`/`Transport` error split.
    pub fn try_clear_queue(&self) -> std::result::Result<PlaybackSnapshot, DaemonCallError> {
        self.try_state_request(&DaemonRequest::ClearQueue)
    }
}

/// Anyhow-flavored convenience wrappers over the `try_*` methods, matching
/// the existing `DaemonClient` surface for future CLI/UI use. No in-tree
/// consumer yet (staticfs needs the `try_*` error split), hence the allow.
#[allow(dead_code)]
impl DaemonClient {
    pub fn settings(&self) -> Result<Vec<(String, String)>> {
        Ok(self.try_settings()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<PlaybackSnapshot> {
        Ok(self.try_set_setting(key, value)?)
    }

    pub fn resolve_and_queue(&self, query: &str) -> Result<PlaybackSnapshot> {
        Ok(self.try_resolve_and_queue(query, false)?)
    }

    pub fn resolve_and_play_radio(&self, query: &str) -> Result<PlaybackSnapshot> {
        Ok(self.try_resolve_and_play_radio(query, false)?)
    }

    pub fn lyrics(&self) -> Result<Option<SyncedLyrics>> {
        Ok(self.try_lyrics()?)
    }

    pub fn clear_queue(&self) -> Result<PlaybackSnapshot> {
        Ok(self.try_clear_queue()?)
    }
}

#[cfg(test)]
#[path = "tests/ipc.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/gain_calibration.rs"]
mod gain_calibration_tests;
