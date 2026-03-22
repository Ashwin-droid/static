use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use gstreamer::ClockTime;
use serde::{Deserialize, Serialize};
use zbus::blocking::{Connection, Proxy};

use crate::logging::LogSource;
use crate::models::{RadioVirtualPlaylist, Track};
use crate::ytmusic::client::SongRating;

pub const DAEMON_BUS_NAME: &str = "com.static.music.staticd";
pub const DAEMON_OBJECT_PATH: &str = "/com/static/music/Staticd";
pub const DAEMON_INTERFACE: &str = "com.static.music.Staticd1";

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
    pub radio_virtual_playlist: Option<RadioVirtualPlaylist>,
    pub repeat_mode: RepeatMode,
    pub paused: bool,
    pub volume: f64,
    pub position_millis: Option<u64>,
    pub duration_millis: Option<u64>,
    pub buffered_fraction: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedPlaybackState {
    pub snapshot: PlaybackSnapshot,
    pub saved_at_unix_millis: u64,
}

impl Default for PlaybackSnapshot {
    fn default() -> Self {
        Self {
            current_track: None,
            queue: Vec::new(),
            queue_index: 0,
            radio_virtual_playlist: None,
            repeat_mode: RepeatMode::Off,
            paused: false,
            volume: 1.0,
            position_millis: None,
            duration_millis: None,
            buffered_fraction: 0.0,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum DaemonRequest {
    Ping,
    GetState,
    PlayQueueAt {
        queue: Vec<Track>,
        index: usize,
        radio_virtual_playlist: Option<RadioVirtualPlaylist>,
        repeat_mode: RepeatMode,
    },
    ReplaceQueue {
        queue: Vec<Track>,
        index: usize,
        radio_virtual_playlist: Option<RadioVirtualPlaylist>,
        repeat_mode: RepeatMode,
    },
    TogglePause,
    Pause,
    Resume,
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
    LogMessage {
        source: LogSource,
        message: String,
    },
    ReloadSettings,
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum DaemonResponse {
    Ok,
    Pong,
    State(PlaybackSnapshot),
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

pub fn playback_state_path() -> PathBuf {
    crate::logging::static_root().join("playback.json")
}

pub fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn load_persisted_playback_state() -> Result<Option<PersistedPlaybackState>> {
    let path = playback_state_path();
    let payload = match fs::read(&path) {
        Ok(payload) => payload,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "Failed to read persisted playback state at {}",
                    path.display()
                )
            });
        }
    };

    serde_json::from_slice(&payload)
        .with_context(|| {
            format!(
                "Failed to parse persisted playback state at {}",
                path.display()
            )
        })
        .map(Some)
}

pub fn store_persisted_playback_state(state: &PersistedPlaybackState) -> Result<()> {
    let path = playback_state_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create playback state dir {}", parent.display()))?;
    }

    let payload = serde_json::to_vec_pretty(state)?;
    let part_path = path.with_file_name(format!(
        "{}.part",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("playback.json")
    ));

    fs::write(&part_path, payload)
        .and_then(|_| fs::rename(&part_path, &path))
        .with_context(|| format!("Failed to persist playback state to {}", path.display()))?;

    Ok(())
}

pub fn clear_persisted_playback_state() -> Result<()> {
    let path = playback_state_path();
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| {
            format!(
                "Failed to clear persisted playback state at {}",
                path.display()
            )
        }),
    }
}

pub fn ensure_daemon_running() -> Result<()> {
    if matches!(
        send_request_raw(&DaemonRequest::Ping),
        Ok(DaemonResponse::Pong)
    ) {
        return Ok(());
    }

    let exe = std::env::current_exe().context("Failed to locate current executable")?;
    Command::new(exe)
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Failed to spawn playback daemon")?;

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        match send_request_raw(&DaemonRequest::Ping) {
            Ok(DaemonResponse::Pong) => return Ok(()),
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }

    bail!("Playback daemon did not become ready in time")
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

    pub fn play_queue_at(
        &self,
        queue: Vec<Track>,
        index: usize,
        radio_virtual_playlist: Option<RadioVirtualPlaylist>,
        repeat_mode: RepeatMode,
    ) -> Result<PlaybackSnapshot> {
        match self.request(&DaemonRequest::PlayQueueAt {
            queue,
            index,
            radio_virtual_playlist,
            repeat_mode,
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
        radio_virtual_playlist: Option<RadioVirtualPlaylist>,
        repeat_mode: RepeatMode,
    ) -> Result<PlaybackSnapshot> {
        match self.request(&DaemonRequest::ReplaceQueue {
            queue,
            index,
            radio_virtual_playlist,
            repeat_mode,
        })? {
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

    pub fn reload_settings(&self) -> Result<PlaybackSnapshot> {
        self.transport_request(DaemonRequest::ReloadSettings)
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
