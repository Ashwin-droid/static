use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use gstreamer as gst;
use gstreamer::ClockTime;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_play as gst_play;

mod audio_filter;
mod dynamic_gain;
mod spectrum;

use dynamic_gain::{Config as GainConfig, DynamicGain};

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        crate::app_log!($($arg)*);
    }};
}

pub struct PlaybackEngine {
    inner: gst_play::Play,
    gain: Arc<Mutex<DynamicGain>>,
    spectrum: Arc<spectrum::Spectrum>,
    visualization_uri: RefCell<Option<String>>,
    master_volume: Cell<f64>,
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
        let gain = Arc::new(Mutex::new(DynamicGain::default()));
        let spectrum = Arc::new(spectrum::Spectrum::default());
        let filter = audio_filter::build(gain.clone(), spectrum.clone())
            .expect("Failed to build playback gain filter");
        pipeline.set_property("audio-filter", &filter);
        inner.set_volume(1.0);

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
            gain,
            spectrum,
            visualization_uri: RefCell::new(None),
            master_volume: Cell::new(1.0),
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
        self.spectrum.reset();
        *self.visualization_uri.borrow_mut() = Some(uri.to_owned());
        // Capture a short first song before GStreamer's flush can clear its history.
        self.gain.lock().unwrap().finish_reference();
        self.inner.stop();
        self.gain.lock().unwrap().reset();
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
        self.spectrum.reset();
        self.inner.pause();
        self.paused.set(true);
    }

    pub fn resume(&self) {
        self.spectrum.reset();
        self.inner.play();
        self.paused.set(false);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.get()
    }

    pub fn stop(&self) {
        self.spectrum.reset();
        self.visualization_uri.borrow_mut().take();
        // Capture a short first song before GStreamer's flush can clear its history.
        self.gain.lock().unwrap().finish_reference();
        self.inner.stop();
        self.gain.lock().unwrap().reset();
        self.paused.set(false);
        self.buf_percent.set(0);
        self.last_error.borrow_mut().take();
        self.cached_position_nanos
            .store(u64::MAX, Ordering::Relaxed);
        self.cached_duration_nanos
            .store(u64::MAX, Ordering::Relaxed);
    }

    pub fn seek(&self, position: ClockTime) {
        self.spectrum.reset();
        self.gain.lock().unwrap().reset();
        self.cached_position_nanos
            .store(position.nseconds(), Ordering::Relaxed);
        self.inner.seek(position);
    }

    pub fn set_volume(&self, volume: f64) {
        if volume.is_finite() {
            let volume = volume.clamp(0.0, 5.0);
            self.master_volume.set(volume);
            self.gain.lock().unwrap().master_gain = volume;
        }
    }

    pub fn volume(&self) -> f64 {
        self.master_volume.get()
    }

    pub fn configure_gain(&self, settings: &crate::offline::AppSettings) {
        self.gain.lock().unwrap().configure(GainConfig {
            enabled: settings.dgain_enabled,
            flatten_percent: settings.dgain_flatten_percent,
            ra_seconds: settings.dgain_ra_seconds,
            change_lazyness_percent: settings.dgain_change_lazyness,
            xdgain_enabled: settings.xdgain_enabled,
        });
    }

    /// A new decoder gets fresh per-song analysis, but keeps the session's
    /// xdGain reference and virtual volume offset through queue transitions.
    pub fn replacement(&self, settings: &crate::offline::AppSettings) -> Self {
        let cross_song = self.gain.lock().unwrap().cross_song_state();
        let replacement = Self::new();
        replacement.configure_gain(settings);
        replacement
            .gain
            .lock()
            .unwrap()
            .restore_cross_song_state(cross_song);
        replacement
    }

    pub fn visualization_frame(&self, bands: usize) -> crate::ipc::VisualizationFrame {
        self.spectrum.frame(
            bands,
            !self.is_paused() && self.pipeline_state() == gst::State::Playing,
        )
    }

    pub fn visualization_uri(&self) -> Option<String> {
        self.visualization_uri.borrow().clone()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_engine_routes_boost_through_filter_with_internal_volume_at_unity() {
        gst::init().unwrap();
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let engine = PlaybackEngine::new();
                let pipeline = engine.inner.pipeline();
                let sink = gst::ElementFactory::make("fakesink")
                    .property("sync", false)
                    .build()
                    .unwrap();
                pipeline.set_property("audio-sink", &sink);
                let filter: gst::Bin = pipeline.property("audio-filter");
                let (send, receive) = std::sync::mpsc::channel();
                filter
                    .by_name("static_gain")
                    .unwrap()
                    .static_pad("src")
                    .unwrap()
                    .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                        if let Some(buffer) = info.buffer() {
                            let map = buffer.map_readable().unwrap();
                            let peak = map
                                .as_slice()
                                .chunks_exact(4)
                                .map(|b| f32::from_ne_bytes(b.try_into().unwrap()).abs())
                                .fold(0.0_f32, f32::max);
                            let _ = send.send(peak);
                        }
                        gst::PadProbeReturn::Ok
                    });
                // A local 100ms stereo WAV, intentionally quiet and with no device I/O.
                let data: Vec<u8> = (0..4800)
                    .flat_map(|frame| {
                        let sample = if frame % 2 == 0 { 3276i16 } else { -3276i16 };
                        [sample, sample].into_iter().flat_map(i16::to_le_bytes)
                    })
                    .collect();
                let mut wav = b"RIFF".to_vec();
                wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
                wav.extend_from_slice(b"WAVEfmt ");
                wav.extend_from_slice(&16u32.to_le_bytes());
                wav.extend_from_slice(&1u16.to_le_bytes());
                wav.extend_from_slice(&2u16.to_le_bytes());
                wav.extend_from_slice(&48_000u32.to_le_bytes());
                wav.extend_from_slice(&192_000u32.to_le_bytes());
                wav.extend_from_slice(&4u16.to_le_bytes());
                wav.extend_from_slice(&16u16.to_le_bytes());
                wav.extend_from_slice(b"data");
                wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
                wav.extend_from_slice(&data);
                let path =
                    std::env::temp_dir().join(format!("static-gain-{}.wav", uuid::Uuid::new_v4()));
                std::fs::write(&path, wav).unwrap();
                engine.set_volume(5.0);
                assert_eq!(engine.volume(), 5.0);
                assert_eq!(engine.inner.volume(), 1.0);
                let uri = glib::filename_to_uri(&path, None).unwrap();
                engine.play_uri(&uri);
                let peak = receive.recv_timeout(std::time::Duration::from_secs(10));
                engine.stop();
                pipeline.set_state(gst::State::Null).unwrap();
                std::fs::remove_file(path).unwrap();
                let peak = peak.expect("Play did not produce audio through its gain filter");
                assert!(
                    (0.49..0.51).contains(&peak),
                    "gain lost in Play pipeline: {peak}"
                );
            })
            .unwrap();
    }

    #[test]
    fn engine_replacement_preserves_xdgain_from_a_short_first_song() {
        gst::init().unwrap();
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let settings = crate::offline::AppSettings {
                    dgain_enabled: true,
                    xdgain_enabled: true,
                    ..Default::default()
                };
                let first = PlaybackEngine::new();
                first.configure_gain(&settings);
                {
                    let mut gain = first.gain.lock().unwrap();
                    gain.set_format(1000, 1);
                    let mut samples: Vec<u8> =
                        (0..500).flat_map(|_| 0.2_f32.to_ne_bytes()).collect();
                    gain.process(&mut samples);
                }
                // A skip before the calibration period ends must still preserve its reference.
                first.stop();
                let next = first.replacement(&settings);
                next.seek(ClockTime::from_seconds(60));
                {
                    let mut gain = next.gain.lock().unwrap();
                    gain.set_format(1000, 1);
                    let mut samples: Vec<u8> =
                        (0..8000).flat_map(|_| 0.02_f32.to_ne_bytes()).collect();
                    gain.process(&mut samples);
                    let last = f32::from_ne_bytes(samples[samples.len() - 4..].try_into().unwrap());
                    assert!(
                        (last - 0.2).abs() < 0.001,
                        "reference lost during decoder replacement: {last}"
                    );
                }
                assert_eq!(next.volume(), 1.0);
                next.stop();
            })
            .unwrap();
    }
}
