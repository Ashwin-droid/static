use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use gstreamer::ClockTime;
use rusqlite::{Connection as SqliteConnection, OptionalExtension};
use serde::{Deserialize, Serialize};
use zbus::blocking::{Connection, Proxy};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use crate::logging::LogSource;
use crate::models::{RadioVirtualPlaylist, Track};
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
    pub paused: bool,
    pub volume: f64,
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
            paused: false,
            volume: 1.0,
            position_millis: None,
            duration_millis: None,
            buffered_fraction: 0.0,
            halt_auto_resume_pending: false,
            probe_active: false,
            probe_restore_position_millis: None,
            sleep_remaining_secs: None,
            sleep_after_current: false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum DaemonRequest {
    Ping,
    GetState,
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
    ActivateRadioChip {
        chip_index: usize,
    },
    SetSleepTimer {
        minutes: u32,
    },
    SetSleepAfterCurrent,
    ClearSleepTimer,
    GetSleepStatus,
    CopyShareUrl,
    LogMessage {
        source: LogSource,
        message: String,
    },
    GetCoordinationTargets,
    ReloadSettings,
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum DaemonResponse {
    Ok,
    Pong,
    State(PlaybackSnapshot),
    CoordinationTargets(CoordinationTargetsSnapshot),
    SleepStatus(SleepStatusSnapshot),
    ShareUrl(String),
    Error(String),
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
    Ok(serde_json::from_str(&response_json)?)
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

pub fn millis_to_clock_time(value: Option<u64>) -> Option<ClockTime> {
    value.map(ClockTime::from_mseconds)
}

#[cfg(test)]
#[path = "tests/ipc.rs"]
mod tests;
