use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use gstreamer::ClockTime;
use rusqlite::{Connection as SqliteConnection, OptionalExtension};
use serde::{Deserialize, Serialize};
use zbus::blocking::{Connection, Proxy};

use crate::logging::LogSource;
use crate::models::{RadioVirtualPlaylist, Track};
use crate::ytmusic::client::SongRating;

pub const DAEMON_BUS_NAME: &str = "com.static.music.staticd";
pub const DAEMON_OBJECT_PATH: &str = "/com/static/music/Staticd";
pub const DAEMON_INTERFACE: &str = "com.static.music.Staticd1";
const PLAYBACK_STATE_DB_FILE_NAME: &str = "data.db";
const PLAYBACK_STATE_TABLE: &str = "playback_state";
const PLAYBACK_STATE_ROW_KEY: &str = "resume";
const PLAYLIST_STATE_ROW_KEY_PREFIX: &str = "playlist";
const PLAYBACK_STATE_DB_BUSY_TIMEOUT: Duration = Duration::from_secs(1);

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
        playlist_context: Option<PlaylistContext>,
        radio_virtual_playlist: Option<RadioVirtualPlaylist>,
        repeat_mode: RepeatMode,
    },
    ReplaceQueue {
        queue: Vec<Track>,
        index: usize,
        playlist_context: Option<PlaylistContext>,
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
        playlist_context: Option<PlaylistContext>,
        radio_virtual_playlist: Option<RadioVirtualPlaylist>,
        repeat_mode: RepeatMode,
    ) -> Result<PlaybackSnapshot> {
        match self.request(&DaemonRequest::PlayQueueAt {
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

#[cfg(test)]
mod tests {
    use super::{
        ActiveTrackSnapshot, PLAYBACK_STATE_ROW_KEY, PersistedPlaybackState, PlaybackSnapshot,
        PlaylistContext, PlaylistContextSource, clear_persisted_state_at_path, current_unix_millis,
        load_persisted_state_at_path, store_persisted_state_at_path,
    };
    use crate::ipc::RepeatMode;
    use crate::models::Track;
    use crate::ytmusic::client::SongRating;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_db_path() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("static-ipc-test-{suffix}"))
            .join("data.db")
    }

    #[test]
    fn persisted_playback_state_round_trips_through_database() {
        let path = unique_temp_db_path();
        let snapshot = PlaybackSnapshot {
            current_track: Some(ActiveTrackSnapshot {
                video_id: "video-123".to_string(),
                title: "Track".to_string(),
                artist: "Artist".to_string(),
                thumb_url: Some("https://example.com/thumb.jpg".to_string()),
                rating: SongRating::Like,
            }),
            queue: Vec::new(),
            queue_index: 0,
            playlist_context: Some(PlaylistContext {
                playlist_id: "PL123".to_string(),
                title: "Coding Mix".to_string(),
                thumbnail_url: Some("https://example.com/playlist.jpg".to_string()),
                subtitle: Some("42 songs".to_string()),
                source: PlaylistContextSource::Remote,
            }),
            radio_virtual_playlist: None,
            repeat_mode: RepeatMode::All,
            paused: true,
            volume: 0.75,
            position_millis: Some(12_345),
            duration_millis: Some(67_890),
            buffered_fraction: 0.5,
        };
        let persisted = PersistedPlaybackState {
            snapshot,
            saved_at_unix_millis: current_unix_millis(),
        };

        store_persisted_state_at_path(&path, PLAYBACK_STATE_ROW_KEY, &persisted).unwrap();
        let loaded = load_persisted_state_at_path(&path, PLAYBACK_STATE_ROW_KEY).unwrap();

        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.saved_at_unix_millis, persisted.saved_at_unix_millis);
        assert_eq!(loaded.snapshot.queue, persisted.snapshot.queue);
        assert_eq!(loaded.snapshot.queue_index, persisted.snapshot.queue_index);
        assert_eq!(
            loaded
                .snapshot
                .playlist_context
                .as_ref()
                .map(|context| context.playlist_id.as_str()),
            persisted
                .snapshot
                .playlist_context
                .as_ref()
                .map(|context| context.playlist_id.as_str())
        );
        assert_eq!(loaded.snapshot.repeat_mode, persisted.snapshot.repeat_mode);
        assert_eq!(loaded.snapshot.paused, persisted.snapshot.paused);
        assert_eq!(loaded.snapshot.volume, persisted.snapshot.volume);
        assert_eq!(
            loaded.snapshot.position_millis,
            persisted.snapshot.position_millis
        );
        assert_eq!(
            loaded.snapshot.duration_millis,
            persisted.snapshot.duration_millis
        );
        assert_eq!(
            loaded.snapshot.buffered_fraction,
            persisted.snapshot.buffered_fraction
        );
        assert_eq!(
            loaded
                .snapshot
                .current_track
                .as_ref()
                .map(|track| &track.video_id),
            persisted
                .snapshot
                .current_track
                .as_ref()
                .map(|track| &track.video_id)
        );

        let _ = fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }

    #[test]
    fn clearing_persisted_playback_state_removes_database_row() {
        let path = unique_temp_db_path();
        let persisted = PersistedPlaybackState {
            snapshot: PlaybackSnapshot::default(),
            saved_at_unix_millis: current_unix_millis(),
        };

        store_persisted_state_at_path(&path, PLAYBACK_STATE_ROW_KEY, &persisted).unwrap();
        clear_persisted_state_at_path(&path, PLAYBACK_STATE_ROW_KEY).unwrap();

        assert!(
            load_persisted_state_at_path(&path, PLAYBACK_STATE_ROW_KEY)
                .unwrap()
                .is_none()
        );

        let _ = fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }

    #[test]
    fn playlist_state_rows_do_not_clobber_global_resume_row() {
        let path = unique_temp_db_path();
        let global = PersistedPlaybackState {
            snapshot: PlaybackSnapshot::default(),
            saved_at_unix_millis: current_unix_millis(),
        };
        let playlist_context = PlaylistContext {
            playlist_id: "PL456".to_string(),
            title: "Late Night".to_string(),
            thumbnail_url: None,
            subtitle: Some("19 songs".to_string()),
            source: PlaylistContextSource::Offline,
        };
        let playlist = PersistedPlaybackState {
            snapshot: PlaybackSnapshot {
                queue: vec![Track {
                    video_id: "track-1".to_string(),
                    title: "One".to_string(),
                    artists: Vec::new(),
                    album: None,
                    duration: None,
                    thumbnail_url: None,
                }],
                queue_index: 0,
                playlist_context: Some(playlist_context.clone()),
                ..PlaybackSnapshot::default()
            },
            saved_at_unix_millis: current_unix_millis(),
        };

        store_persisted_state_at_path(&path, PLAYBACK_STATE_ROW_KEY, &global).unwrap();
        store_persisted_state_at_path(&path, &playlist_context.storage_key(), &playlist).unwrap();

        let loaded_global = load_persisted_state_at_path(&path, PLAYBACK_STATE_ROW_KEY).unwrap();
        let loaded_playlist =
            load_persisted_state_at_path(&path, &playlist_context.storage_key()).unwrap();

        assert!(loaded_global.is_some());
        assert!(loaded_playlist.is_some());
        assert_eq!(loaded_global.unwrap().snapshot.queue.len(), 0);
        assert_eq!(loaded_playlist.unwrap().snapshot.queue.len(), 1);

        let _ = fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
}
