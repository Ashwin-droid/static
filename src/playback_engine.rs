use std::cell::Cell;
use std::rc::Rc;

use gstreamer as gst;
use gstreamer::ClockTime;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_play as gst_play;

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        crate::app_log!($($arg)*);
    }};
}

pub struct PlaybackEngine {
    inner: gst_play::Play,
    paused: Cell<bool>,
    buf_percent: Rc<Cell<u32>>,
    _bus_watch: gst::bus::BusWatchGuard,
}

impl PlaybackEngine {
    pub fn new() -> Self {
        let inner = gst_play::Play::new(None::<gst_play::PlayVideoRenderer>);
        let pipeline = inner.pipeline();
        pipeline.set_property("ring-buffer-max-size", 200u64 * 1024 * 1024);

        let buf_percent = Rc::new(Cell::new(0u32));
        let bp = buf_percent.clone();
        let bus = inner.message_bus();
        let bus_watch = bus
            .add_watch_local(move |_, msg| {
                if let Ok(play_msg) = gst_play::PlayMessage::parse(msg) {
                    if let gst_play::PlayMessage::Buffering(buffering) = play_msg {
                        bp.set(buffering.percent());
                    }
                }

                match msg.view() {
                    gst::MessageView::Error(err) => {
                        eprintln!(
                            "[PLAYER] gstreamer error from {}: {} ({})",
                            err.src()
                                .map(|src| src.path_string())
                                .unwrap_or_else(|| "unknown".into()),
                            err.error(),
                            err.debug().unwrap_or_else(|| "no debug details".into())
                        );
                    }
                    gst::MessageView::Eos(..) => {
                        eprintln!("[PLAYER] reached end of stream");
                    }
                    _ => {}
                }
                glib::ControlFlow::Continue
            })
            .expect("Failed to add bus watch");

        Self {
            inner,
            paused: Cell::new(false),
            buf_percent,
            _bus_watch: bus_watch,
        }
    }

    pub fn play_uri(&self, uri: &str) {
        self.inner.stop();
        self.buf_percent.set(0);
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
        self.buf_percent.set(0);
    }

    pub fn seek(&self, position: ClockTime) {
        self.inner.seek(position);
    }

    pub fn set_volume(&self, volume: f64) {
        self.inner.set_volume(volume);
    }

    pub fn volume(&self) -> f64 {
        self.inner.volume()
    }

    pub fn position(&self) -> Option<ClockTime> {
        self.inner.position()
    }

    pub fn duration(&self) -> Option<ClockTime> {
        self.inner.duration()
    }

    pub fn buffered_fraction(&self) -> f64 {
        (self.buf_percent.get() as f64 / 100.0).clamp(0.0, 1.0)
    }
}
