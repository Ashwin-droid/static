use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

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
    last_error: Rc<RefCell<Option<String>>>,
    cached_position_nanos: Arc<AtomicU64>,
    cached_duration_nanos: Arc<AtomicU64>,
    _signal_adapter: gst_play::PlaySignalAdapter,
    _bus_watch: gst::bus::BusWatchGuard,
}

impl PlaybackEngine {
    pub fn new() -> Self {
        let inner = gst_play::Play::new(None::<gst_play::PlayVideoRenderer>);
        let pipeline = inner.pipeline();
        pipeline.set_property("ring-buffer-max-size", 200u64 * 1024 * 1024);

        let buf_percent = Rc::new(Cell::new(0u32));
        let bp = buf_percent.clone();
        let last_error = Rc::new(RefCell::new(None));
        let last_error_for_watch = last_error.clone();
        let cached_position_nanos = Arc::new(AtomicU64::new(u64::MAX));
        let cached_duration_nanos = Arc::new(AtomicU64::new(u64::MAX));
        let signal_adapter = gst_play::PlaySignalAdapter::new_sync_emit(&inner);

        {
            let cached_position_nanos = cached_position_nanos.clone();
            signal_adapter.connect_position_updated(move |_, position| {
                cached_position_nanos.store(
                    position
                        .map(|position| position.nseconds())
                        .unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
            });
        }

        {
            let cached_position_nanos = cached_position_nanos.clone();
            signal_adapter.connect_seek_done(move |_, position| {
                cached_position_nanos.store(position.nseconds(), Ordering::Relaxed);
            });
        }

        {
            let cached_duration_nanos = cached_duration_nanos.clone();
            signal_adapter.connect_duration_changed(move |_, duration| {
                cached_duration_nanos.store(
                    duration
                        .map(|duration| duration.nseconds())
                        .unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
            });
        }

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
                        let source = err
                            .src()
                            .map(|src| src.path_string().to_string())
                            .unwrap_or_else(|| "unknown".to_string());
                        let message = format!(
                            "{source}: {} ({})",
                            err.error(),
                            err.debug()
                                .map(|debug| debug.to_string())
                                .unwrap_or_else(|| "no debug details".to_string())
                        );
                        *last_error_for_watch.borrow_mut() = Some(message.clone());
                        eprintln!(
                            "[PLAYER] gstreamer error from {}: {} ({})",
                            source,
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
            last_error,
            cached_position_nanos,
            cached_duration_nanos,
            _signal_adapter: signal_adapter,
            _bus_watch: bus_watch,
        }
    }

    pub fn play_uri(&self, uri: &str) {
        self.inner.stop();
        self.buf_percent.set(0);
        self.last_error.borrow_mut().take();
        self.cached_position_nanos
            .store(u64::MAX, Ordering::Relaxed);
        self.cached_duration_nanos
            .store(u64::MAX, Ordering::Relaxed);
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

    pub fn is_paused(&self) -> bool {
        self.paused.get()
    }

    pub fn stop(&self) {
        self.inner.stop();
        self.paused.set(false);
        self.buf_percent.set(0);
        self.last_error.borrow_mut().take();
        self.cached_position_nanos
            .store(u64::MAX, Ordering::Relaxed);
        self.cached_duration_nanos
            .store(u64::MAX, Ordering::Relaxed);
    }

    pub fn seek(&self, position: ClockTime) {
        self.cached_position_nanos
            .store(position.nseconds(), Ordering::Relaxed);
        self.inner.seek(position);
    }

    pub fn set_volume(&self, volume: f64) {
        self.inner.set_volume(volume);
    }

    pub fn volume(&self) -> f64 {
        self.inner.volume()
    }

    pub fn position(&self) -> Option<ClockTime> {
        atomic_nanos_to_clock_time(&self.cached_position_nanos).or_else(|| self.inner.position())
    }

    pub fn duration(&self) -> Option<ClockTime> {
        atomic_nanos_to_clock_time(&self.cached_duration_nanos).or_else(|| self.inner.duration())
    }

    pub fn buffered_fraction(&self) -> f64 {
        (self.buf_percent.get() as f64 / 100.0).clamp(0.0, 1.0)
    }

    pub fn pipeline_state(&self) -> gst::State {
        self.inner.pipeline().current_state()
    }

    pub fn take_error(&self) -> Option<String> {
        self.last_error.borrow_mut().take()
    }
}

fn atomic_nanos_to_clock_time(nanos: &AtomicU64) -> Option<ClockTime> {
    let nanos = nanos.load(Ordering::Relaxed);
    (nanos != u64::MAX).then(|| ClockTime::from_nseconds(nanos))
}
