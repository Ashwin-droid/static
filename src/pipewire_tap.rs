//! Dynamic PipeWire ducking: the duck follows how loud the foreign audio is.
//!
//! Each coordinated foreign stream is tapped with a native PipeWire capture
//! stream (`target.object = <serial>`, `stream.capture.sink = true`, which
//! captures exactly that playback stream's output). All taps live on one
//! PipeWire loop thread. The process callback keeps a mean-square level over
//! the latest 20 ms window and wakes the daemon whenever the level rises, so
//! louder foreign audio ducks the music without waiting for the next
//! maintenance tick. The daemon combines the streams' energies, follows the
//! level with an instant attack and a linear gain-back release, and maps it
//! through a fixed CSS-style ease curve to a duck amount between the user's
//! minimum and maximum duck. There is no speech model: the mapping is a fixed
//! level window chosen so that meeting voices reach the maximum duck and
//! gaps between sentences fall back to the minimum.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use tokio::sync::Notify;

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        crate::app_log!($($arg)*);
    }};
}

/// Foreign audio at or below this short-term level is "quiet": minimum duck.
pub const QUIET_DBFS: f64 = -50.0;
/// Foreign audio at or above this short-term level is "loud": maximum duck.
pub const LOUD_DBFS: f64 = -20.0;
/// Length of one level measurement.
const WINDOW: Duration = Duration::from_millis(20);
/// A tap that has not delivered a window for this long is treated as silent.
const STALE_AFTER: Duration = Duration::from_millis(400);
/// A new tap that has not delivered its first window yet is still connecting.
/// Past this it counts as dead and the duck falls back to the maximum.
const CONNECT_GRACE: Duration = Duration::from_secs(1);
/// After the capture thread loses PipeWire, wait this long before reconnecting.
const RESTART_BACKOFF: Duration = Duration::from_secs(2);
/// Used until the stream reports its negotiated rate.
const FALLBACK_RATE: u32 = 48_000;
const SILENCE_DBFS: f64 = -100.0;

/// CSS `ease-in-out` (cubic): slow out of the quiet end, slow into the loud
/// end, so faint background noise barely moves the music and a voice that is
/// already clearly there does not keep pumping it.
pub fn ease_in_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Duck amount (0 = untouched, 1 = silent) for a foreign level in dBFS.
pub fn duck_amount_for_level(level_dbfs: f64, min_duck: f64, max_duck: f64) -> f64 {
    let (min_duck, max_duck) = ordered_ducks(min_duck, max_duck);
    let t = (level_dbfs - QUIET_DBFS) / (LOUD_DBFS - QUIET_DBFS);
    min_duck + (max_duck - min_duck) * ease_in_out(t)
}

pub fn ordered_ducks(min_duck: f64, max_duck: f64) -> (f64, f64) {
    let min_duck = min_duck.clamp(0.0, 1.0);
    let max_duck = max_duck.clamp(0.0, 1.0);
    if min_duck <= max_duck {
        (min_duck, max_duck)
    } else {
        (max_duck, min_duck)
    }
}

/// Smoothed foreign level in dBFS. Louder input is taken at once, so the
/// duck deepens immediately. Quieter input is approached linearly, at a rate
/// where crossing the whole quiet..loud window takes exactly `gain_back`;
/// through the ease curve that makes the music climb back ease-in-out.
#[derive(Debug, Clone, Copy)]
pub struct LevelFollower {
    level_dbfs: f64,
    last_update: Option<Instant>,
}

impl Default for LevelFollower {
    fn default() -> Self {
        Self {
            level_dbfs: SILENCE_DBFS,
            last_update: None,
        }
    }
}

impl LevelFollower {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn level_dbfs(&self) -> f64 {
        self.level_dbfs
    }

    /// Whether a level has been fed since the last reset.
    pub fn has_level(&self) -> bool {
        self.last_update.is_some()
    }

    /// Feed the instantaneous combined level measured at `now`.
    pub fn update(&mut self, instant_dbfs: f64, now: Instant, gain_back: Duration) -> f64 {
        let instant_dbfs = instant_dbfs.max(SILENCE_DBFS);
        let dt = self.last_update.map_or(0.0, |last| {
            now.saturating_duration_since(last).as_secs_f64()
        });
        self.last_update = Some(now);
        if instant_dbfs >= self.level_dbfs || gain_back.is_zero() {
            self.level_dbfs = instant_dbfs;
            return self.level_dbfs;
        }
        // Above the window the duck is already at its maximum, so the climb
        // starts from the loud edge instead of first holding up there.
        let rate = (LOUD_DBFS - QUIET_DBFS) / gain_back.as_secs_f64();
        self.level_dbfs = (self.level_dbfs.min(LOUD_DBFS) - rate * dt).max(instant_dbfs);
        self.level_dbfs
    }
}

#[derive(Debug, Default)]
struct WindowLevel {
    mean_square: f64,
    updated_at: Option<Instant>,
    /// The capture stream failed or was torn down: tap is dead.
    failed: bool,
}

/// Splits captured f32 samples into fixed windows.
#[derive(Debug)]
struct WindowMeter {
    window_samples: usize,
    energy: f64,
    samples: usize,
}

impl WindowMeter {
    fn new(rate: u32, channels: u32) -> Self {
        let per_second = rate.max(1) as u128 * channels.max(1) as u128;
        Self {
            window_samples: (per_second * WINDOW.as_millis() / 1000).max(1) as usize,
            energy: 0.0,
            samples: 0,
        }
    }

    /// Mean square of the loudest window completed by `bytes` (little-endian
    /// f32), or `None` if no window completed.
    fn push(&mut self, bytes: &[u8]) -> Option<f64> {
        let mut loudest: Option<f64> = None;
        for sample in bytes.chunks_exact(4) {
            let value = f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]) as f64;
            if value.is_finite() {
                self.energy += value * value;
            }
            self.samples += 1;
            if self.samples >= self.window_samples {
                let mean_square = self.energy / self.samples as f64;
                loudest = Some(loudest.map_or(mean_square, |loudest| loudest.max(mean_square)));
                self.energy = 0.0;
                self.samples = 0;
            }
        }
        loudest
    }
}

struct Tap {
    level: Arc<Mutex<WindowLevel>>,
    tapped_at: Instant,
}

/// Owns one native capture stream per coordinated foreign stream.
pub struct ForeignLevelTaps {
    taps: HashMap<u32, Tap>,
    capture: Option<CaptureThread>,
    restart_after: Option<Instant>,
    wake: Arc<Notify>,
}

impl Default for ForeignLevelTaps {
    fn default() -> Self {
        Self {
            taps: HashMap::new(),
            capture: None,
            restart_after: None,
            wake: Arc::new(Notify::new()),
        }
    }
}

impl ForeignLevelTaps {
    /// Notified (coalesced) whenever a tap measures a window louder than its
    /// previous one.
    pub fn wake(&self) -> Arc<Notify> {
        self.wake.clone()
    }

    /// Make the tap set match `streams` (node id, object serial), tapping and
    /// untapping as needed. Capture targets by serial, not node id.
    pub fn sync(&mut self, streams: &[(u32, u32)]) {
        let now = Instant::now();
        if self
            .capture
            .as_ref()
            .is_some_and(|capture| !capture.is_alive())
        {
            self.capture = None;
            self.taps.clear();
            self.restart_after = Some(now + RESTART_BACKOFF);
        }
        let removed: Vec<u32> = self
            .taps
            .keys()
            .copied()
            .filter(|node_id| !streams.iter().any(|(node, _)| node == node_id))
            .collect();
        for node_id in removed {
            self.taps.remove(&node_id);
            if let Some(capture) = &self.capture {
                capture.send(CaptureCommand::Untap { node_id });
            }
        }
        for &(node_id, serial) in streams {
            if self.taps.contains_key(&node_id) {
                continue;
            }
            let Some(capture) = self.capture_thread(now) else {
                return;
            };
            let level = Arc::new(Mutex::new(WindowLevel::default()));
            capture.send(CaptureCommand::Tap {
                node_id,
                serial,
                level: level.clone(),
            });
            self.taps.insert(
                node_id,
                Tap {
                    level,
                    tapped_at: now,
                },
            );
        }
    }

    /// Drop every tap. The capture thread stays connected for the next duck.
    pub fn clear(&mut self) {
        if let Some(capture) = &self.capture {
            for &node_id in self.taps.keys() {
                capture.send(CaptureCommand::Untap { node_id });
            }
        }
        self.taps.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.taps.is_empty()
    }

    /// Combined instantaneous level of every live tap, in dBFS. `None` when
    /// no tap is delivering audio (all dead, still connecting, or none
    /// tapped): see [`Self::connecting`] before falling back to a guess.
    pub fn combined_level_dbfs(&self, now: Instant) -> Option<f64> {
        let mut energy = 0.0;
        let mut live = false;
        for tap in self.taps.values() {
            let level = tap.level.lock().unwrap();
            if level.failed {
                continue;
            }
            match level.updated_at {
                Some(at) if now.saturating_duration_since(at) <= STALE_AFTER => {
                    live = true;
                    energy += level.mean_square;
                }
                Some(_) => live = true, // silent for now, but the tap is alive
                None => {}
            }
        }
        live.then(|| 10.0 * energy.max(1e-10).log10())
    }

    /// True while some tap is still waiting for its first window. The caller
    /// should hold the duck rather than jump to the maximum and then have to
    /// climb back once the real level arrives.
    pub fn connecting(&self, now: Instant) -> bool {
        self.taps.values().any(|tap| {
            let level = tap.level.lock().unwrap();
            !level.failed
                && level.updated_at.is_none()
                && now.saturating_duration_since(tap.tapped_at) < CONNECT_GRACE
        })
    }

    fn capture_thread(&mut self, now: Instant) -> Option<&CaptureThread> {
        if self.capture.is_none() {
            if self.restart_after.is_some_and(|after| now < after) {
                return None;
            }
            self.restart_after = None;
            match CaptureThread::spawn(self.wake.clone()) {
                Some(capture) => self.capture = Some(capture),
                None => {
                    self.restart_after = Some(now + RESTART_BACKOFF);
                    return None;
                }
            }
        }
        self.capture.as_ref()
    }
}

enum CaptureCommand {
    Tap {
        node_id: u32,
        serial: u32,
        level: Arc<Mutex<WindowLevel>>,
    },
    Untap {
        node_id: u32,
    },
    Quit,
}

/// The PipeWire loop thread that owns every capture stream.
struct CaptureThread {
    commands: pw::channel::Sender<CaptureCommand>,
    alive: Arc<AtomicBool>,
}

impl CaptureThread {
    fn spawn(wake: Arc<Notify>) -> Option<Self> {
        let (commands, receiver) = pw::channel::channel();
        let alive = Arc::new(AtomicBool::new(true));
        let alive_for_thread = alive.clone();
        std::thread::Builder::new()
            .name("duck-capture".to_string())
            .spawn(move || {
                if let Err(err) = run_capture(receiver, wake) {
                    eprintln!("[PIPEWIRE] duck capture stopped: {err}");
                }
                alive_for_thread.store(false, Ordering::SeqCst);
            })
            .map_err(|err| eprintln!("[PIPEWIRE] failed to start duck capture thread: {err}"))
            .ok()?;
        Some(Self { commands, alive })
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn send(&self, command: CaptureCommand) {
        let _ = self.commands.send(command);
    }
}

impl Drop for CaptureThread {
    fn drop(&mut self) {
        self.send(CaptureCommand::Quit);
    }
}

struct CaptureTap {
    // Declared first so the listener unhooks before the stream is destroyed.
    _listener: pw::stream::StreamListener<TapData>,
    _stream: pw::stream::Stream,
}

struct TapData {
    meter: WindowMeter,
    level: Arc<Mutex<WindowLevel>>,
    previous_mean_square: f64,
    wake: Arc<Notify>,
}

fn run_capture(
    commands: pw::channel::Receiver<CaptureCommand>,
    wake: Arc<Notify>,
) -> Result<(), pw::Error> {
    pw::init();
    let main_loop = pw::main_loop::MainLoop::new(None)?;
    let context = pw::context::Context::new(&main_loop)?;
    let core = context.connect(None)?;
    let taps = Rc::new(RefCell::new(HashMap::<u32, CaptureTap>::new()));

    let main_loop_weak = main_loop.downgrade();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _, res, message| {
            if id == 0 {
                eprintln!("[PIPEWIRE] duck capture lost PipeWire: {res} ({message})");
                if let Some(main_loop) = main_loop_weak.upgrade() {
                    main_loop.quit();
                }
            }
        })
        .register();

    let main_loop_weak = main_loop.downgrade();
    let core_for_commands = core.clone();
    let taps_for_commands = taps.clone();
    let _commands = commands.attach(main_loop.loop_(), move |command| match command {
        CaptureCommand::Tap {
            node_id,
            serial,
            level,
        } => match connect_tap(&core_for_commands, serial, level.clone(), wake.clone()) {
            Ok(tap) => {
                taps_for_commands.borrow_mut().insert(node_id, tap);
            }
            Err(err) => {
                eprintln!("[PIPEWIRE] failed to tap node {node_id}: {err}");
                level.lock().unwrap().failed = true;
            }
        },
        CaptureCommand::Untap { node_id } => {
            taps_for_commands.borrow_mut().remove(&node_id);
        }
        CaptureCommand::Quit => {
            if let Some(main_loop) = main_loop_weak.upgrade() {
                main_loop.quit();
            }
        }
    });

    main_loop.run();
    taps.borrow_mut().clear();
    Ok(())
}

fn connect_tap(
    core: &pw::core::Core,
    serial: u32,
    level: Arc<Mutex<WindowLevel>>,
    wake: Arc<Notify>,
) -> Result<CaptureTap, pw::Error> {
    let props = properties! {
        "media.type" => "Audio",
        "media.category" => "Capture",
        "media.role" => "Production",
        "node.name" => "static-duck-tap",
        "target.object" => serial.to_string(),
        "stream.capture.sink" => "true",
        // Never fall back to another source: the default sink monitor would
        // be Static's own music, and the default source the user's mic.
        "node.dont-fallback" => "true",
        "node.dont-reconnect" => "true",
    };
    let stream = pw::stream::Stream::new(core, "static-duck-tap", props)?;
    let data = TapData {
        meter: WindowMeter::new(FALLBACK_RATE, 1),
        level,
        previous_mean_square: 0.0,
        wake,
    };
    let listener = stream
        .add_local_listener_with_user_data(data)
        .param_changed(|_, data, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = spa::param::audio::AudioInfoRaw::new();
            if info.parse(param).is_ok() && info.rate() > 0 {
                data.meter = WindowMeter::new(info.rate(), info.channels());
            }
        })
        .state_changed(|_, data, _, state| {
            if matches!(
                state,
                pw::stream::StreamState::Error(_) | pw::stream::StreamState::Unconnected
            ) {
                // A tap that already heard its stream lost it because the
                // stream ended: that is silence, not a reason to duck fully.
                let mut level = data.level.lock().unwrap();
                if level.updated_at.is_none() {
                    level.failed = true;
                }
            }
        })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(block) = buffer.datas_mut().first_mut() else {
                return;
            };
            let offset = block.chunk().offset() as usize;
            let size = block.chunk().size() as usize;
            let Some(bytes) = block.data() else {
                return;
            };
            let start = offset.min(bytes.len());
            let end = start.saturating_add(size).min(bytes.len());
            let Some(mean_square) = data.meter.push(&bytes[start..end]) else {
                return;
            };
            {
                let mut level = data.level.lock().unwrap();
                level.mean_square = mean_square;
                level.updated_at = Some(Instant::now());
            }
            if mean_square > data.previous_mean_square {
                data.wake.notify_one();
            }
            data.previous_mean_square = mean_square;
        })
        .register()?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_channels(1);
    let mut position = [0; 64];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    info.set_position(position);
    let format = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        }),
    )
    .map_err(|_| pw::Error::CreationFailed)?
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&format).ok_or(pw::Error::CreationFailed)?];
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::DONT_RECONNECT,
        &mut params,
    )?;
    Ok(CaptureTap {
        _listener: listener,
        _stream: stream,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_curve_hits_both_ends_and_is_monotonic() {
        assert_eq!(ease_in_out(0.0), 0.0);
        assert_eq!(ease_in_out(1.0), 1.0);
        assert!((ease_in_out(0.5) - 0.5).abs() < 1e-12);
        let mut last = 0.0;
        for i in 1..=100 {
            let v = ease_in_out(i as f64 / 100.0);
            assert!(v >= last);
            last = v;
        }
        // Flat ends: faint noise barely moves it.
        assert!(ease_in_out(0.1) < 0.01);
        assert!(ease_in_out(0.9) > 0.99);
    }

    #[test]
    fn duck_amount_spans_min_to_max_across_the_level_window() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(duck_amount_for_level(-80.0, 0.3, 0.85), 0.3));
        assert!(close(duck_amount_for_level(QUIET_DBFS, 0.3, 0.85), 0.3));
        assert!(close(duck_amount_for_level(LOUD_DBFS, 0.3, 0.85), 0.85));
        assert!(close(duck_amount_for_level(-5.0, 0.3, 0.85), 0.85));
        let mid = duck_amount_for_level((QUIET_DBFS + LOUD_DBFS) / 2.0, 0.3, 0.85);
        assert!(close(mid, 0.575));
        // Swapped settings are tolerated.
        assert!(close(duck_amount_for_level(-80.0, 0.85, 0.3), 0.3));
    }

    #[test]
    fn follower_drops_instantly_from_any_level() {
        let t0 = Instant::now();
        let gain_back = Duration::from_secs(5);
        let mut follower = LevelFollower::default();
        assert_eq!(follower.update(-60.0, t0, gain_back), -60.0);
        // Quiet to loud in one step, no attack time.
        assert_eq!(follower.update(-20.0, t0, gain_back), -20.0);
        // Already ducked, then louder still: deeper at once.
        assert_eq!(follower.update(-10.0, t0, gain_back), -10.0);
    }

    #[test]
    fn follower_gains_back_across_the_window_in_the_configured_time() {
        let t0 = Instant::now();
        let gain_back = Duration::from_secs(5);
        let mut follower = LevelFollower::default();
        // Start well above the window: the climb begins at the loud edge.
        follower.update(-5.0, t0, gain_back);
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let halfway = follower.update(-90.0, at(2500), gain_back);
        assert!(
            (halfway - (LOUD_DBFS + QUIET_DBFS) / 2.0).abs() < 1e-9,
            "{halfway}"
        );
        let done = follower.update(-90.0, at(5000), gain_back);
        assert!((done - QUIET_DBFS).abs() < 1e-9, "{done}");
        // Through the ease curve that is the full max..min duck in 5 s.
        assert!((duck_amount_for_level(done, 0.3, 0.85) - 0.3).abs() < 1e-9);
        // A short gap between words barely moves the duck.
        let mut follower = LevelFollower::default();
        follower.update(-20.0, t0, gain_back);
        let gap = follower.update(-90.0, at(300), gain_back);
        assert!(duck_amount_for_level(gap, 0.3, 0.85) > 0.84, "{gap}");
    }

    #[test]
    fn follower_gain_back_never_undershoots_the_input() {
        let t0 = Instant::now();
        let mut follower = LevelFollower::default();
        follower.update(-20.0, t0, Duration::from_secs(1));
        let level = follower.update(-30.0, t0 + Duration::from_secs(10), Duration::from_secs(1));
        assert_eq!(level, -30.0);
        // Zero gain back follows the input straight down.
        let level = follower.update(-45.0, t0 + Duration::from_secs(10), Duration::ZERO);
        assert_eq!(level, -45.0);
    }

    #[test]
    fn meter_reports_the_loudest_completed_window() {
        let samples = |value: f32, count: usize| -> Vec<u8> {
            std::iter::repeat_n(value.to_le_bytes(), count)
                .flatten()
                .collect()
        };
        let mut meter = WindowMeter::new(1000, 1); // 20 samples per window
        assert_eq!(meter.push(&samples(0.5, 19)), None);
        let mut bytes = samples(0.5, 1);
        bytes.extend(samples(0.1, 20));
        bytes.extend(samples(0.0, 5));
        let loudest = meter.push(&bytes).unwrap();
        assert!((loudest - 0.25).abs() < 1e-9, "{loudest}");
        // The partial window carries over.
        assert_eq!(meter.push(&samples(0.0, 14)), None);
        assert_eq!(meter.push(&samples(0.0, 1)), Some(0.0));
    }

    #[test]
    fn taps_report_none_without_streams() {
        let taps = ForeignLevelTaps::default();
        assert!(taps.combined_level_dbfs(Instant::now()).is_none());
        assert!(!taps.connecting(Instant::now()));
    }
}
