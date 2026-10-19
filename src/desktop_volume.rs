//! Desktop aware volume.
//!
//! When PipeWire coordination ducks or pauses Static for foreign audio, the
//! user often cranks the system sink to hear that quieter content. Once
//! Static comes back, this module (a) compensates Static's own output so it
//! returns at the loudness it had before the interruption, and (b) after a
//! quiet hold, slides the sink back to where it was while the compensation
//! fades out, so the two moves cancel and nothing audible changes.
//!
//! Ground rules enforced here:
//! - the sink is identified by node id; if the default sink changes, the job
//!   is abandoned and only the compensation fades out (never touch another
//!   device's volume);
//! - once Static is alone again, any sink change we did not make ourselves
//!   abandons the job (never fight the user);
//! - the hold clock only runs while Static is actually playing unsuppressed.
//!   A user pause freezes the job instead of abandoning it; a sink change
//!   made during that pause re-bases the compensation when playback resumes.
//!
//! All sink IO (wpctl) lives in `read_default_sink`/`write_sink_volume`; the
//! state machine is pure and driven by `tick`/`observe_sample`.

use std::collections::VecDeque;
use std::process::Command;
use std::time::{Duration, Instant};

/// Sink samples retained while idle, to pick a baseline from before the
/// foreign stream first appeared.
const BASELINE_HISTORY: Duration = Duration::from_secs(90);
/// wpctl prints two decimals; anything beyond rounding is a user touch.
const SINK_TOUCH_TOLERANCE: f64 = 0.015;
/// Below this delta nothing audible happened and no job is worth running.
const MIN_DELTA_DB: f64 = 0.5;
const MIN_SINK_VOLUME: f64 = 0.0001;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SinkSample {
    pub node_id: u32,
    /// wpctl's cubic volume (1.0 = unity, 1.5 = +150%).
    pub volume: f64,
    pub muted: bool,
    pub at: Instant,
}

/// Sink gain in dB. wpctl's displayed volume is cubic: amplitude = v^3.
pub fn sink_db(volume: f64) -> f64 {
    60.0 * volume.max(MIN_SINK_VOLUME).log10()
}

pub fn db_to_sink_volume(db: f64) -> f64 {
    10.0_f64.powf(db / 60.0)
}

fn db_to_amplitude(db: f64) -> f64 {
    10.0_f64.powf(db / 20.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopVolumeInputs {
    pub now: Instant,
    pub enabled: bool,
    /// Foreign audio is currently active on the sink (trigger window).
    pub foreign_active: bool,
    /// Coordination has engaged (ducked or PipeWire-paused). Jobs start here.
    pub suppressed: bool,
    /// Static has a track and is playing with no suppression of any kind.
    pub playing_alone: bool,
    /// When the foreign audio that engaged coordination first appeared.
    pub foreign_since: Option<Instant>,
    pub hold: Duration,
    pub restore: Duration,
}

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    /// Foreign audio active; nothing to do but watch the sink identity.
    Interrupted,
    /// Foreign audio stopped; waiting for Static to come back (resume delay).
    Resuming,
    /// Static is back, compensated; waiting out the quiet hold.
    Hold {
        held: Duration,
        last_tick: Option<Instant>,
    },
    /// Sliding the sink back and the compensation out together.
    Restore {
        elapsed: Duration,
        last_tick: Option<Instant>,
        start_sink_db: f64,
        start_comp_db: f64,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct Job {
    sink_node: u32,
    baseline_db: f64,
    /// Sink level we expect to read back; anything else is a user touch.
    expected_sink: Option<f64>,
    /// True while Static is playing alone, i.e. while touches count.
    watching: bool,
    /// Latest sink level seen while the job was frozen by a user pause.
    paused_sink: Option<f64>,
    phase: Phase,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct FadeOut {
    elapsed: Duration,
    last_tick: Option<Instant>,
    start_comp_db: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopVolumePhase {
    Interrupted,
    Resuming,
    Hold,
    Restore,
    FadeOut,
}

impl DesktopVolumePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interrupted => "interrupted",
            Self::Resuming => "resuming",
            Self::Hold => "hold",
            Self::Restore => "restore",
            Self::FadeOut => "fade_out",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DesktopVolumeStatus {
    pub phase: DesktopVolumePhase,
    pub comp_db: f64,
    pub sink_delta_db: f64,
    pub remaining: Option<Duration>,
}

#[derive(Debug, Default)]
pub struct DesktopAwareVolume {
    history: VecDeque<SinkSample>,
    job: Option<Job>,
    fade_out: Option<FadeOut>,
    comp_db: f64,
    /// Sink volume the next IO cycle should write (to `sink_node`).
    pending_write: Option<(u32, f64)>,
    was_suppressed: bool,
}

impl DesktopAwareVolume {
    /// Amplitude multiplier applied to Static's output volume.
    pub fn comp_gain(&self) -> f64 {
        db_to_amplitude(self.comp_db)
    }

    #[cfg(test)]
    pub fn comp_db(&self) -> f64 {
        self.comp_db
    }

    #[cfg(test)]
    pub fn is_active(&self) -> bool {
        self.job.is_some() || self.fade_out.is_some()
    }

    /// Whether the daemon should keep sampling the sink right now.
    pub fn wants_samples(&self, inputs: &DesktopVolumeInputs) -> bool {
        inputs.enabled && (self.job.is_some() || inputs.playing_alone || inputs.suppressed)
    }

    /// The sink write the next IO cycle must perform before reading.
    pub fn take_pending_write(&mut self) -> Option<(u32, f64)> {
        self.pending_write.take()
    }

    pub fn status(&self) -> Option<DesktopVolumeStatus> {
        if let Some(job) = &self.job {
            let (phase, remaining) = match &job.phase {
                Phase::Interrupted => (DesktopVolumePhase::Interrupted, None),
                Phase::Resuming => (DesktopVolumePhase::Resuming, None),
                Phase::Hold { .. } => (DesktopVolumePhase::Hold, None),
                Phase::Restore { .. } => (DesktopVolumePhase::Restore, None),
            };
            let sink_delta_db = job
                .expected_sink
                .map_or(0.0, |expected| sink_db(expected) - job.baseline_db);
            return Some(DesktopVolumeStatus {
                phase,
                comp_db: self.comp_db,
                sink_delta_db,
                remaining,
            });
        }
        self.fade_out.map(|_| DesktopVolumeStatus {
            phase: DesktopVolumePhase::FadeOut,
            comp_db: self.comp_db,
            sink_delta_db: 0.0,
            remaining: None,
        })
    }

    /// Remaining hold/restore time, for status surfaces.
    pub fn remaining(&self, inputs: &DesktopVolumeInputs) -> Option<Duration> {
        match self.job.as_ref().map(|job| &job.phase) {
            Some(Phase::Hold { held, .. }) => Some(inputs.hold.saturating_sub(*held)),
            Some(Phase::Restore { elapsed, .. }) => Some(inputs.restore.saturating_sub(*elapsed)),
            _ => self
                .fade_out
                .map(|fade| inputs.restore.saturating_sub(fade.elapsed)),
        }
    }

    /// Feed a sink reading. `None` means the sink could not be read.
    pub fn observe_sample(&mut self, sample: Option<SinkSample>) {
        let Some(sample) = sample else {
            // Can't see the sink: never act blind.
            if self.job.is_some() {
                self.abandon();
            }
            return;
        };
        self.history.push_back(sample);
        while self
            .history
            .front()
            .is_some_and(|oldest| sample.at.duration_since(oldest.at) > BASELINE_HISTORY)
        {
            self.history.pop_front();
        }

        let Some(job) = self.job.as_mut() else {
            return;
        };
        if sample.node_id != job.sink_node {
            self.abandon();
            return;
        }
        match &mut job.phase {
            Phase::Interrupted | Phase::Resuming => {}
            Phase::Hold { .. } | Phase::Restore { .. } => {
                if !job.watching {
                    // Frozen by a user pause: remember, re-base on resume.
                    job.paused_sink = Some(sample.volume);
                    return;
                }
                let touched = sample.muted
                    || job.expected_sink.is_some_and(|expected| {
                        (sample.volume - expected).abs() > SINK_TOUCH_TOLERANCE
                    });
                if touched {
                    self.abandon();
                }
            }
        }
    }

    /// Advance the state machine. Call once per daemon maintenance tick.
    pub fn tick(&mut self, inputs: &DesktopVolumeInputs) {
        let became_suppressed = inputs.suppressed && !self.was_suppressed;
        self.was_suppressed = inputs.suppressed;

        if !inputs.enabled {
            if self.job.is_some() {
                self.abandon();
            }
            self.pending_write = None;
            self.advance_fade_out(inputs);
            return;
        }

        if self.job.is_none() {
            if became_suppressed {
                self.start_job(inputs);
            }
        }

        if self.job.is_none() {
            self.advance_fade_out(inputs);
            return;
        }
        // A duck-mode unduck lands Static back alone in the same tick the
        // foreign audio vanishes, so Interrupted -> Resuming -> Hold must be
        // able to happen within one call.
        while self.tick_job(inputs) {}
    }

    /// One phase step; returns true when the phase changed and the new phase
    /// should be evaluated against the same inputs right away.
    fn tick_job(&mut self, inputs: &DesktopVolumeInputs) -> bool {
        let Some(job) = self.job.as_mut() else {
            return false;
        };

        match &mut job.phase {
            Phase::Interrupted => {
                if !inputs.foreign_active {
                    job.phase = Phase::Resuming;
                    return true;
                }
            }
            Phase::Resuming => {
                if inputs.foreign_active {
                    job.phase = Phase::Interrupted;
                    return false;
                }
                // The compensation is applied EAGERLY, while Static is still
                // suppressed, so every resume path (calm ramp, immediate
                // resume, MPRIS side-channel resume, fresh track start)
                // already sees the compensated steady level. Computing it
                // on the tick after the resume let the first ~40 ms play at
                // the cranked level.
                //
                // The goal is Static's loudness staying what it was, not
                // matching the video, so the only inputs are the sink before
                // the interruption and the sink now.
                let current = self
                    .history
                    .back()
                    .filter(|sample| sample.node_id == job.sink_node)
                    .map(|sample| sample.volume);
                let Some(current) = current else {
                    self.abandon();
                    return false;
                };
                let current_db = sink_db(current);
                self.comp_db = if (current_db - job.baseline_db).abs() < MIN_DELTA_DB {
                    // The user already put the sink back: nothing to do.
                    0.0
                } else {
                    job.baseline_db - current_db
                };
                self.fade_out = None;
                if inputs.playing_alone {
                    if self.comp_db.abs() < 1e-9 {
                        self.abandon();
                        return false;
                    }
                    job.expected_sink = Some(current);
                    job.watching = true;
                    job.paused_sink = None;
                    job.phase = Phase::Hold {
                        held: Duration::ZERO,
                        last_tick: Some(inputs.now),
                    };
                }
            }
            Phase::Hold { held, last_tick } => {
                if inputs.foreign_active {
                    job.phase = Phase::Interrupted;
                    job.expected_sink = None;
                    job.watching = false;
                    job.paused_sink = None;
                } else if inputs.playing_alone {
                    if !job.watching {
                        job.watching = true;
                        if let Some(moved) = job.paused_sink.take() {
                            if job.expected_sink.is_none_or(|expected| {
                                (moved - expected).abs() > SINK_TOUCH_TOLERANCE
                            }) {
                                self.comp_db = job.baseline_db - sink_db(moved);
                                job.expected_sink = Some(moved);
                            }
                        }
                    }
                    if let Some(last) = last_tick {
                        *held += inputs.now.saturating_duration_since(*last);
                    }
                    *last_tick = Some(inputs.now);
                    if *held >= inputs.hold {
                        let start_sink_db = job.expected_sink.map_or(job.baseline_db, sink_db);
                        job.phase = Phase::Restore {
                            elapsed: Duration::ZERO,
                            last_tick: Some(inputs.now),
                            start_sink_db,
                            start_comp_db: self.comp_db,
                        };
                    }
                } else {
                    *last_tick = None;
                    job.watching = false;
                }
            }
            Phase::Restore {
                elapsed,
                last_tick,
                start_sink_db,
                start_comp_db,
            } => {
                if inputs.foreign_active {
                    job.phase = Phase::Interrupted;
                    job.expected_sink = None;
                    job.watching = false;
                    job.paused_sink = None;
                } else if inputs.playing_alone {
                    if !job.watching {
                        job.watching = true;
                        if let Some(moved) = job.paused_sink.take() {
                            if job.expected_sink.is_none_or(|expected| {
                                (moved - expected).abs() > SINK_TOUCH_TOLERANCE
                            }) {
                                // Re-base and restart the slide from where the
                                // user left the sink.
                                self.comp_db = job.baseline_db - sink_db(moved);
                                job.expected_sink = Some(moved);
                                *elapsed = Duration::ZERO;
                                *start_sink_db = sink_db(moved);
                                *start_comp_db = self.comp_db;
                            }
                        }
                    }
                    if let Some(last) = last_tick {
                        *elapsed += inputs.now.saturating_duration_since(*last);
                    }
                    *last_tick = Some(inputs.now);
                    let progress = if inputs.restore.is_zero() {
                        1.0
                    } else {
                        (elapsed.as_secs_f64() / inputs.restore.as_secs_f64()).clamp(0.0, 1.0)
                    };
                    let target_sink_db =
                        *start_sink_db + (job.baseline_db - *start_sink_db) * progress;
                    self.comp_db = *start_comp_db * (1.0 - progress);
                    let target_volume = (db_to_sink_volume(target_sink_db) * 100.0).round() / 100.0;
                    if job
                        .expected_sink
                        .is_none_or(|expected| (expected - target_volume).abs() > 1e-9)
                    {
                        job.expected_sink = Some(target_volume);
                        self.pending_write = Some((job.sink_node, target_volume));
                    }
                    if progress >= 1.0 {
                        self.comp_db = 0.0;
                        self.job = None;
                    }
                } else {
                    *last_tick = None;
                    job.watching = false;
                }
            }
        }
        false
    }

    fn start_job(&mut self, inputs: &DesktopVolumeInputs) {
        let baseline = inputs
            .foreign_since
            .and_then(|since| {
                self.history
                    .iter()
                    .rev()
                    .find(|sample| sample.at <= since && !sample.muted)
            })
            .or_else(|| self.history.iter().find(|sample| !sample.muted))
            .copied();
        let Some(baseline) = baseline else {
            return;
        };
        self.fade_out = None;
        self.job = Some(Job {
            sink_node: baseline.node_id,
            baseline_db: sink_db(baseline.volume),
            expected_sink: None,
            watching: false,
            paused_sink: None,
            phase: Phase::Interrupted,
        });
    }

    /// Drop the job; the sink stays wherever it is and only the compensation
    /// fades out.
    fn abandon(&mut self) {
        self.job = None;
        self.pending_write = None;
        if self.comp_db.abs() > 1e-9 && self.fade_out.is_none() {
            self.fade_out = Some(FadeOut {
                elapsed: Duration::ZERO,
                last_tick: None,
                start_comp_db: self.comp_db,
            });
        }
    }

    fn advance_fade_out(&mut self, inputs: &DesktopVolumeInputs) {
        let Some(fade) = self.fade_out.as_mut() else {
            return;
        };
        if let Some(last) = fade.last_tick {
            fade.elapsed += inputs.now.saturating_duration_since(last);
        }
        fade.last_tick = Some(inputs.now);
        let progress = if inputs.restore.is_zero() {
            1.0
        } else {
            (fade.elapsed.as_secs_f64() / inputs.restore.as_secs_f64()).clamp(0.0, 1.0)
        };
        self.comp_db = fade.start_comp_db * (1.0 - progress);
        if progress >= 1.0 {
            self.comp_db = 0.0;
            self.fade_out = None;
        }
    }
}

/// Read the default sink through wpctl. Blocking; call off the daemon loop.
pub fn read_default_sink() -> Option<SinkSample> {
    let inspect = Command::new("wpctl")
        .args(["inspect", "@DEFAULT_AUDIO_SINK@"])
        .output()
        .ok()?;
    if !inspect.status.success() {
        return None;
    }
    let node_id = parse_inspect_node_id(&String::from_utf8_lossy(&inspect.stdout))?;
    let volume = Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .output()
        .ok()?;
    if !volume.status.success() {
        return None;
    }
    let (volume, muted) = parse_get_volume(&String::from_utf8_lossy(&volume.stdout))?;
    Some(SinkSample {
        node_id,
        volume,
        muted,
        at: Instant::now(),
    })
}

/// Set a sink's volume by node id (never by the default alias, so a device
/// switch between read and write can't redirect the write).
pub fn write_sink_volume(node_id: u32, volume: f64) -> bool {
    Command::new("wpctl")
        .args([
            "set-volume",
            &node_id.to_string(),
            &format!("{:.2}", volume.clamp(0.0, 1.5)),
        ])
        .status()
        .is_ok_and(|status| status.success())
}

fn parse_inspect_node_id(output: &str) -> Option<u32> {
    let first = output.lines().next()?.trim();
    let rest = first.strip_prefix("id ")?;
    let id = rest.split(',').next()?.trim();
    id.parse().ok()
}

fn parse_get_volume(output: &str) -> Option<(f64, bool)> {
    let line = output.lines().find(|line| line.contains("Volume:"))?;
    let rest = line.split("Volume:").nth(1)?.trim();
    let mut parts = rest.split_whitespace();
    let volume: f64 = parts.next()?.parse().ok()?;
    let muted = rest.contains("[MUTED]");
    Some((volume, muted))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(node: u32, volume: f64, at: Instant) -> SinkSample {
        SinkSample {
            node_id: node,
            volume,
            muted: false,
            at,
        }
    }

    fn inputs(now: Instant) -> DesktopVolumeInputs {
        DesktopVolumeInputs {
            now,
            enabled: true,
            foreign_active: false,
            suppressed: false,
            playing_alone: true,
            foreign_since: None,
            hold: Duration::from_secs(300),
            restore: Duration::from_secs(60),
        }
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Drive the canonical timeline: quiet at 0.30, interruption from t=60 to
    /// t=180 during which the sink is cranked to 0.60, resume delay until
    /// t=240. Returns (machine, t0).
    fn run_canonical(machine: &mut DesktopAwareVolume, t0: Instant) {
        run_canonical_checked(machine, t0, true);
    }

    fn run_canonical_checked(machine: &mut DesktopAwareVolume, t0: Instant, check: bool) {
        for t in 0..60 {
            let now = t0 + secs(t);
            machine.observe_sample(Some(sample(7, 0.30, now)));
            machine.tick(&inputs(now));
        }
        let foreign_since = t0 + secs(60);
        for t in 60..180 {
            let now = t0 + secs(t);
            let mut i = inputs(now);
            i.foreign_active = true;
            i.suppressed = true;
            i.playing_alone = false;
            i.foreign_since = Some(foreign_since);
            // Cranked halfway through, then held at 0.60.
            let volume = if t < 90 { 0.30 } else { 0.60 };
            machine.tick(&i);
            machine.observe_sample(Some(sample(7, volume, now)));
            if check {
                assert_eq!(
                    machine.comp_db(),
                    0.0,
                    "nothing applied while foreign audio plays"
                );
            }
        }
        // Resume delay: foreign gone, Static still suppressed.
        for t in 180..240 {
            let now = t0 + secs(t);
            let mut i = inputs(now);
            i.suppressed = true;
            i.playing_alone = false;
            machine.tick(&i);
            machine.observe_sample(Some(sample(7, 0.60, now)));
        }
    }

    #[test]
    fn compensation_is_baseline_minus_sink_at_resume() {
        let t0 = Instant::now();
        let mut machine = DesktopAwareVolume::default();
        run_canonical(&mut machine, t0);
        // Eager: the compensation is already in place during the resume
        // delay, so whichever path resumes playback lands on it.
        let expected = sink_db(0.30) - sink_db(0.60);
        assert!(
            (machine.comp_db() - expected).abs() < 1e-6,
            "{}",
            machine.comp_db()
        );
        assert_eq!(
            machine.status().unwrap().phase,
            DesktopVolumePhase::Resuming
        );

        let now = t0 + secs(240);
        machine.tick(&inputs(now));
        assert!(
            (machine.comp_db() - expected).abs() < 1e-6,
            "{}",
            machine.comp_db()
        );
        assert!(machine.comp_gain() < 1.0);
        assert_eq!(machine.status().unwrap().phase, DesktopVolumePhase::Hold);
    }

    #[test]
    fn downward_crank_yields_positive_compensation() {
        let t0 = Instant::now();
        let mut machine = DesktopAwareVolume::default();
        for t in 0..10 {
            let now = t0 + secs(t);
            machine.observe_sample(Some(sample(7, 0.60, now)));
            machine.tick(&inputs(now));
        }
        for t in 10..60 {
            let now = t0 + secs(t);
            let mut i = inputs(now);
            i.foreign_active = true;
            i.suppressed = true;
            i.playing_alone = false;
            i.foreign_since = Some(t0 + secs(10));
            machine.tick(&i);
            machine.observe_sample(Some(sample(7, 0.30, now)));
        }
        machine.tick(&inputs(t0 + secs(60)));
        assert!(machine.comp_db() > 0.0);
        assert!(machine.comp_gain() > 1.0);
    }

    #[test]
    fn hold_only_counts_time_playing_alone_and_then_restores_sink_and_comp_together() {
        let t0 = Instant::now();
        let mut machine = DesktopAwareVolume::default();
        run_canonical(&mut machine, t0);
        let mut t = 240;
        machine.tick(&inputs(t0 + secs(t)));
        let comp_at_resume = machine.comp_db();

        // User pauses Static for 100 s: the clock must not run.
        for _ in 0..100 {
            t += 1;
            let mut i = inputs(t0 + secs(t));
            i.playing_alone = false;
            machine.tick(&i);
            machine.observe_sample(Some(sample(7, 0.60, t0 + secs(t))));
        }
        assert_eq!(machine.status().unwrap().phase, DesktopVolumePhase::Hold);

        // 300 ticks after the pause = 299 s counted (the first tick only
        // restarts the clock): still holding.
        for _ in 0..300 {
            t += 1;
            machine.tick(&inputs(t0 + secs(t)));
            machine.observe_sample(Some(sample(7, 0.60, t0 + secs(t))));
            assert!(machine.take_pending_write().is_none());
        }
        assert_eq!(machine.status().unwrap().phase, DesktopVolumePhase::Hold);
        assert_eq!(machine.comp_db(), comp_at_resume);

        // Crossing the hold starts the restore.
        t += 1;
        machine.tick(&inputs(t0 + secs(t)));
        assert_eq!(machine.status().unwrap().phase, DesktopVolumePhase::Restore);

        let mut last_sink = 0.60;
        let mut last_comp = comp_at_resume;
        for _ in 0..60 {
            t += 1;
            machine.tick(&inputs(t0 + secs(t)));
            if let Some((node, volume)) = machine.take_pending_write() {
                assert_eq!(node, 7);
                assert!(
                    volume <= last_sink + 1e-9,
                    "sink must only move toward baseline"
                );
                last_sink = volume;
                // The sample we read back is exactly what we wrote: not a touch.
                machine.observe_sample(Some(sample(7, volume, t0 + secs(t))));
            } else {
                machine.observe_sample(Some(sample(7, last_sink, t0 + secs(t))));
            }
            assert!(
                machine.comp_db() >= last_comp - 1e-9,
                "comp must only move toward 0 dB"
            );
            last_comp = machine.comp_db();
        }
        assert!((last_sink - 0.30).abs() < 1e-9, "{last_sink}");
        assert_eq!(machine.comp_db(), 0.0);
        assert!(!machine.is_active());
    }

    #[test]
    fn sink_change_during_a_user_pause_rebases_comp_instead_of_abandoning() {
        let t0 = Instant::now();
        let mut machine = DesktopAwareVolume::default();
        run_canonical(&mut machine, t0);
        let mut t = 240;
        machine.tick(&inputs(t0 + secs(t)));
        assert_eq!(machine.status().unwrap().phase, DesktopVolumePhase::Hold);

        // Pause Static, then nudge the sink further up while paused.
        for _ in 0..30 {
            t += 1;
            let mut i = inputs(t0 + secs(t));
            i.playing_alone = false;
            machine.tick(&i);
            machine.observe_sample(Some(sample(7, 0.80, t0 + secs(t))));
        }
        assert_eq!(machine.status().unwrap().phase, DesktopVolumePhase::Hold);

        // Resume: comp now cancels the 0.30 -> 0.80 delta and the job lives on.
        t += 1;
        machine.tick(&inputs(t0 + secs(t)));
        let expected = sink_db(0.30) - sink_db(0.80);
        assert!(
            (machine.comp_db() - expected).abs() < 1e-6,
            "{}",
            machine.comp_db()
        );
        assert_eq!(machine.status().unwrap().phase, DesktopVolumePhase::Hold);
        // And the new level is what we expect from now on: not a touch.
        machine.observe_sample(Some(sample(7, 0.80, t0 + secs(t))));
        assert_eq!(machine.status().unwrap().phase, DesktopVolumePhase::Hold);
    }

    #[test]
    fn user_touching_the_sink_during_hold_abandons_and_fades_comp_out() {
        let t0 = Instant::now();
        let mut machine = DesktopAwareVolume::default();
        run_canonical(&mut machine, t0);
        machine.tick(&inputs(t0 + secs(240)));
        let comp = machine.comp_db();
        assert!(comp < 0.0);

        machine.observe_sample(Some(sample(7, 0.45, t0 + secs(241))));
        assert_eq!(machine.status().unwrap().phase, DesktopVolumePhase::FadeOut);
        assert!(machine.take_pending_write().is_none());

        for t in 241..=301 {
            machine.tick(&inputs(t0 + secs(t)));
            assert!(
                machine.take_pending_write().is_none(),
                "never write after abandon"
            );
        }
        assert_eq!(machine.comp_db(), 0.0);
        assert!(!machine.is_active());
    }

    #[test]
    fn sink_switch_abandons_without_touching_the_new_device() {
        let t0 = Instant::now();
        let mut machine = DesktopAwareVolume::default();
        run_canonical(&mut machine, t0);
        machine.tick(&inputs(t0 + secs(240)));
        // Earbuds appear as the default sink at the same level.
        machine.observe_sample(Some(sample(42, 0.60, t0 + secs(241))));
        assert_eq!(machine.status().unwrap().phase, DesktopVolumePhase::FadeOut);
        for t in 241..=600 {
            machine.tick(&inputs(t0 + secs(t)));
            assert!(machine.take_pending_write().is_none());
        }
        assert!(!machine.is_active());
    }

    #[test]
    fn reinterruption_during_hold_keeps_the_original_baseline() {
        let t0 = Instant::now();
        let mut machine = DesktopAwareVolume::default();
        run_canonical(&mut machine, t0);
        let mut t = 240;
        machine.tick(&inputs(t0 + secs(t)));

        // Second video at t=300..360, cranked further to 0.90.
        for _ in 0..60 {
            t += 1;
            let mut i = inputs(t0 + secs(t));
            i.foreign_active = true;
            i.suppressed = true;
            i.playing_alone = false;
            i.foreign_since = Some(t0 + secs(300));
            machine.tick(&i);
            machine.observe_sample(Some(sample(7, 0.90, t0 + secs(t))));
        }
        assert_eq!(
            machine.status().unwrap().phase,
            DesktopVolumePhase::Interrupted
        );
        t += 1;
        machine.tick(&inputs(t0 + secs(t)));
        let expected = sink_db(0.30) - sink_db(0.90);
        assert!((machine.comp_db() - expected).abs() < 1e-6);
        assert_eq!(machine.status().unwrap().phase, DesktopVolumePhase::Hold);
    }

    #[test]
    fn sink_already_back_at_baseline_at_resume_means_no_job() {
        let t0 = Instant::now();
        let mut machine = DesktopAwareVolume::default();
        run_canonical(&mut machine, t0);
        // During the resume delay the user turned it back down themselves.
        machine.observe_sample(Some(sample(7, 0.30, t0 + secs(239))));
        machine.tick(&inputs(t0 + secs(240)));
        assert_eq!(machine.comp_db(), 0.0);
        assert!(!machine.is_active());
    }

    #[test]
    fn disabling_the_setting_mid_job_fades_comp_and_leaves_the_sink() {
        let t0 = Instant::now();
        let mut machine = DesktopAwareVolume::default();
        run_canonical(&mut machine, t0);
        machine.tick(&inputs(t0 + secs(240)));
        let mut i = inputs(t0 + secs(241));
        i.enabled = false;
        machine.tick(&i);
        assert_eq!(machine.status().unwrap().phase, DesktopVolumePhase::FadeOut);
        let mut i = inputs(t0 + secs(302));
        i.enabled = false;
        machine.tick(&i);
        assert_eq!(machine.comp_db(), 0.0);
        assert!(machine.take_pending_write().is_none());
    }

    #[test]
    fn wpctl_output_parses() {
        assert_eq!(
            parse_inspect_node_id("id 127, type PipeWire:Interface:Node\n  foo = bar\n"),
            Some(127)
        );
        assert_eq!(parse_get_volume("Volume: 0.28\n"), Some((0.28, false)));
        assert_eq!(
            parse_get_volume("Volume: 1.00 [MUTED]\n"),
            Some((1.0, true))
        );
        assert_eq!(parse_get_volume("garbage"), None);
    }

    #[test]
    fn sink_db_is_cubic() {
        assert!((sink_db(1.0)).abs() < 1e-9);
        assert!((sink_db(0.5) - (-18.0618)).abs() < 1e-3);
        assert!((db_to_sink_volume(sink_db(0.37)) - 0.37).abs() < 1e-9);
    }
}
