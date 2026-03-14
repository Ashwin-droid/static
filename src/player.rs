use std::cell::RefCell;

use gstreamer::ClockTime;

use crate::ipc::{DaemonClient, PlaybackSnapshot, RepeatMode, millis_to_clock_time};
use crate::models::{RadioVirtualPlaylist, Track};

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        crate::app_log!($($arg)*);
    }};
}

pub struct Player {
    client: DaemonClient,
    snapshot: RefCell<PlaybackSnapshot>,
}

impl Player {
    pub fn new() -> Self {
        let client = DaemonClient::new().expect("Failed to connect to staticd");
        let snapshot = client.state().unwrap_or_default();
        Self {
            client,
            snapshot: RefCell::new(snapshot),
        }
    }

    pub fn snapshot(&self) -> PlaybackSnapshot {
        self.snapshot.borrow().clone()
    }

    pub fn set_queue(
        &self,
        queue: Vec<Track>,
        index: usize,
        radio_virtual_playlist: Option<RadioVirtualPlaylist>,
        repeat_mode: RepeatMode,
    ) -> Result<PlaybackSnapshot, String> {
        self.run_command(|client| {
            client.replace_queue(queue, index, radio_virtual_playlist, repeat_mode)
        })
    }

    pub fn replace_queue(
        &self,
        queue: Vec<Track>,
        index: usize,
        radio_virtual_playlist: Option<RadioVirtualPlaylist>,
        repeat_mode: RepeatMode,
    ) -> Result<PlaybackSnapshot, String> {
        self.set_queue(queue, index, radio_virtual_playlist, repeat_mode)
    }

    pub fn toggle_pause(&self) -> Result<PlaybackSnapshot, String> {
        self.run_command(|client| client.toggle_pause())
    }

    pub fn stop(&self) -> Result<PlaybackSnapshot, String> {
        self.run_command(|client| client.stop())
    }

    pub fn seek(&self, position: ClockTime) -> Result<PlaybackSnapshot, String> {
        let position_millis = position.nseconds() / 1_000_000;
        self.run_command(|client| client.seek_to_millis(position_millis))
    }

    pub fn set_volume(&self, volume: f64) -> Result<PlaybackSnapshot, String> {
        self.run_command(|client| client.set_volume(volume))
    }

    pub fn set_repeat_mode(&self, repeat_mode: RepeatMode) -> Result<PlaybackSnapshot, String> {
        self.run_command(|client| client.set_repeat_mode(repeat_mode))
    }

    pub fn position(&self) -> Option<ClockTime> {
        millis_to_clock_time(self.snapshot.borrow().position_millis)
    }

    pub fn duration(&self) -> Option<ClockTime> {
        millis_to_clock_time(self.snapshot.borrow().duration_millis)
    }

    pub fn buffered_fraction(&self) -> f64 {
        self.snapshot.borrow().buffered_fraction
    }

    pub fn daemon_client(&self) -> DaemonClient {
        self.client.clone()
    }

    pub fn apply_snapshot(&self, snapshot: PlaybackSnapshot) {
        *self.snapshot.borrow_mut() = snapshot;
    }

    fn run_command<F>(&self, f: F) -> Result<PlaybackSnapshot, String>
    where
        F: FnOnce(&DaemonClient) -> anyhow::Result<PlaybackSnapshot>,
    {
        match f(&self.client) {
            Ok(snapshot) => {
                self.apply_snapshot(snapshot.clone());
                Ok(snapshot)
            }
            Err(err) => Err(err.to_string()),
        }
    }
}

pub fn notify_daemon_settings_changed() {
    match DaemonClient::new().and_then(|client| client.reload_settings()) {
        Ok(_) => {}
        Err(err) => eprintln!("[PLAYER] failed to notify staticd about settings changes: {err}"),
    }
}
