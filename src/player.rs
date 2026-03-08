use std::cell::Cell;

use gstreamer::ClockTime;
use gstreamer_play as gst_play;

pub struct Player {
    inner: gst_play::Play,
    paused: Cell<bool>,
}

impl Player {
    pub fn new() -> Self {
        let inner = gst_play::Play::new(None::<gst_play::PlayVideoRenderer>);
        Self {
            inner,
            paused: Cell::new(false),
        }
    }

    pub fn play_uri(&self, uri: &str) {
        self.inner.set_uri(Some(uri));
        self.inner.play();
        self.paused.set(false);
    }

    pub fn pause(&self) {
        self.inner.pause();
        self.paused.set(true);
    }

    pub fn resume(&self) {
        self.inner.play();
        self.paused.set(false);
    }

    pub fn toggle_pause(&self) {
        if self.paused.get() {
            self.resume();
        } else {
            self.pause();
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.get()
    }

    pub fn stop(&self) {
        self.inner.stop();
        self.paused.set(false);
    }

    pub fn seek(&self, position: ClockTime) {
        self.inner.seek(position);
    }

    pub fn set_volume(&self, volume: f64) {
        self.inner.set_volume(volume);
    }

    #[allow(dead_code)]
    pub fn volume(&self) -> f64 {
        self.inner.volume()
    }

    pub fn position(&self) -> Option<ClockTime> {
        self.inner.position()
    }

    pub fn duration(&self) -> Option<ClockTime> {
        self.inner.duration()
    }
}
