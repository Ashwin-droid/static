//! Linked-channel gain processing. All timing is measured in decoded audio frames.
use std::collections::VecDeque;

const SILENCE_RMS: f64 = 0.0001;
const MAX_DYNAMIC_GAIN: f64 = 5.0;
const OUTPUT_CEILING: f64 = 0.999;
const XDGAIN_REFERENCE_SECONDS: f64 = 3.0;
const XDGAIN_MIN_DB: f64 = -36.0;
const XDGAIN_MAX_DB: f64 = 24.0;

/// Session state, carried into replacement engines independently of song history.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct CrossSongGain {
    target_rms: Option<f64>,
    offset_db: f64,
}

pub(super) use crate::ipc::GainCalibrationSettings as Config;
use crate::ipc::GainDiagnostics;
use std::sync::atomic::{AtomicU64, Ordering};
static DIAGNOSTIC_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
struct Observation {
    frames: usize,
    // Silence occupies time in the window, but is not a loudness reference.
    audible: bool,
    mean_square: f64,
}

/// Gain intent computed from original audio, before either gain stage.
#[derive(Clone, Copy, Debug)]
pub(super) struct GainTarget {
    pub dynamic: f64,
    pub offset_db: f64,
    pub audible: bool,
    pub peak: f64,
    pub rms: f64,
    pub average: f64,
    pub window_seconds: f64,
    pub audible_seconds: f64,
}

pub(super) struct DynamicGain {
    config: Config,
    diagnostics: GainDiagnostics,
    sample_rate: usize,
    channels: usize,
    history: VecDeque<Observation>,
    history_frames: usize,
    audible_frames: usize,
    energy: f64,
    dynamic_gain: f64,
    limiter_gain: f64,
    cross_song: CrossSongGain,
    reference_seconds: f64,
    reference_energy: f64,
    pub master_gain: f64,
    revision: u64,
    stream_revision: u64,
    envelope_started: bool,
    held_dynamic_db: f64,
    held_offset_db: f64,
}

impl Default for DynamicGain {
    fn default() -> Self {
        Self {
            config: Config::default(),
            diagnostics: GainDiagnostics {
                generation: DIAGNOSTIC_GENERATION.fetch_add(1, Ordering::Relaxed),
                ..Default::default()
            },
            sample_rate: 48_000,
            channels: 2,
            history: VecDeque::new(),
            history_frames: 0,
            audible_frames: 0,
            energy: 0.0,
            dynamic_gain: 1.0,
            limiter_gain: 1.0,
            cross_song: CrossSongGain::default(),
            reference_seconds: 0.0,
            reference_energy: 0.0,
            master_gain: 1.0,
            revision: 0,
            stream_revision: 0,
            envelope_started: false,
            held_dynamic_db: 0.0,
            held_offset_db: 0.0,
        }
    }
}

impl DynamicGain {
    pub fn configure(&mut self, config: Config) {
        if self.config != config {
            if self.config.xdgain_enabled != config.xdgain_enabled {
                self.cross_song = CrossSongGain::default();
            }
            self.config = config;
            self.reset_history();
        }
    }

    pub fn set_format(&mut self, sample_rate: usize, channels: usize) {
        if self.sample_rate != sample_rate || self.channels != channels {
            self.sample_rate = sample_rate.max(1);
            self.channels = channels.max(1);
            self.reset();
        }
    }

    pub fn reset(&mut self) {
        self.stream_revision = self.stream_revision.wrapping_add(1);
        self.reset_history();
    }

    fn reset_history(&mut self) {
        self.diagnostics = GainDiagnostics {
            generation: DIAGNOSTIC_GENERATION.fetch_add(1, Ordering::Relaxed),
            ..Default::default()
        };
        self.revision = self.revision.wrapping_add(1);
        self.envelope_started = false;
        self.history.clear();
        self.history_frames = 0;
        self.audible_frames = 0;
        self.energy = 0.0;
        self.dynamic_gain = 1.0;
        self.limiter_gain = 1.0;
        self.reference_seconds = 0.0;
        self.reference_energy = 0.0;
    }

    /// If the first song ends early, use the audible evidence it supplied.
    pub fn finish_reference(&mut self) {
        if self.cross_song.target_rms.is_none() && self.reference_seconds > 0.0 {
            self.cross_song.target_rms =
                Some((self.reference_energy / self.reference_seconds).sqrt());
        }
    }

    pub fn cross_song_state(&mut self) -> CrossSongGain {
        self.finish_reference();
        self.cross_song
    }

    pub fn restore_cross_song_state(&mut self, state: CrossSongGain) {
        self.cross_song = state;
    }

    fn desired_cross_song_offset(&mut self, rms: f64, average: f64, frames: usize) -> f64 {
        // Silence neither becomes the reference nor drives gain toward its ceiling.
        if rms < SILENCE_RMS || average < SILENCE_RMS {
            return self.cross_song.offset_db;
        }
        if self.cross_song.target_rms.is_none() {
            let seconds = (frames as f64 / self.sample_rate as f64)
                .min((XDGAIN_REFERENCE_SECONDS - self.reference_seconds).max(0.0));
            self.reference_seconds += seconds;
            self.reference_energy += rms * rms * seconds;
            if self.reference_seconds + 1e-9 >= XDGAIN_REFERENCE_SECONDS {
                self.finish_reference();
            }
        }
        self.cross_song.target_rms.map_or(0.0, |reference| {
            (20.0 * (reference / average).log10()).clamp(XDGAIN_MIN_DB, XDGAIN_MAX_DB)
        })
    }

    fn observe(&mut self, frames: usize, mean_square: f64) -> f64 {
        let audible = mean_square >= SILENCE_RMS * SILENCE_RMS;
        self.history.push_back(Observation {
            frames,
            audible,
            mean_square,
        });
        self.history_frames += frames;
        if audible {
            self.audible_frames += frames;
            self.energy += mean_square * frames as f64;
        }
        let window = (self.config.ra_seconds * self.sample_rate as f64)
            .round()
            .max(1.0) as usize;
        while self.history_frames > window {
            let excess = self.history_frames - window;
            let oldest = self.history.front_mut().expect("nonempty audio history");
            let remove = excess.min(oldest.frames);
            self.history_frames -= remove;
            oldest.frames -= remove;
            if oldest.audible {
                self.audible_frames -= remove;
                self.energy -= oldest.mean_square * remove as f64;
            }
            if oldest.frames == 0 {
                self.history.pop_front();
            }
        }
        if self.audible_frames == 0 {
            0.0
        } else {
            (self.energy.max(0.0) / self.audible_frames as f64).sqrt()
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn stream_revision(&self) -> u64 {
        self.stream_revision
    }

    pub fn format(&self) -> (usize, usize) {
        (self.sample_rate, self.channels)
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Analyze each original audio block exactly once, as it enters lookahead.
    /// The rolling reference advances in audio time independently of master gain.
    pub fn analyze(&mut self, bytes: &[u8]) -> GainTarget {
        let frames = bytes.len() / (self.channels * 4);
        let mut square_sum = 0.0;
        let mut peak = 0.0_f64;
        for sample in bytes.chunks_exact(4) {
            let value = f32::from_ne_bytes(sample.try_into().unwrap()) as f64;
            let value = if value.is_finite() { value } else { 0.0 };
            square_sum += value * value;
            peak = peak.max(value.abs());
        }
        let rms = if frames == 0 {
            0.0
        } else {
            (square_sum / (frames * self.channels) as f64).sqrt()
        };
        let average = if self.config.enabled && frames > 0 {
            self.observe(frames, rms * rms)
        } else {
            0.0
        };
        let audible = rms >= SILENCE_RMS && average >= SILENCE_RMS;
        let dynamic = if self.config.enabled && audible {
            let tolerance = 1.0 + self.config.flatten_percent / 100.0;
            (rms.clamp(average / tolerance, average * tolerance) / rms)
                .clamp(0.000001, MAX_DYNAMIC_GAIN)
        } else {
            1.0
        };
        let offset_db = if self.config.enabled && self.config.xdgain_enabled {
            self.desired_cross_song_offset(rms, average, frames)
        } else {
            self.cross_song.offset_db
        };
        GainTarget {
            dynamic,
            offset_db,
            audible,
            peak,
            rms,
            average,
            window_seconds: self.history_frames as f64 / self.sample_rate as f64,
            audible_seconds: self.audible_frames as f64 / self.sample_rate as f64,
        }
    }

    pub fn diagnostics(&self) -> GainDiagnostics {
        GainDiagnostics {
            settings: self.config,
            master_gain: self.master_gain,
            reference_rms: self.cross_song.target_rms,
            ..self.diagnostics.clone()
        }
    }

    pub fn set_diagnostic_position(&mut self, millis: Option<u64>) {
        self.diagnostics.position_millis = millis;
    }

    /// Apply the most conservative upcoming target to the current buffered audio.
    /// Attack and release run per sample in dB, preserving linked stereo channels.
    /// Only peak protection may attenuate immediately; normal dynamics never step
    /// at block boundaries. A 250ms preview gives the 35ms attack time to settle
    /// before a loud entry rather than clamping on the entry itself.
    pub fn render(&mut self, bytes: &mut [u8], current: GainTarget, upcoming: GainTarget) {
        let attack = 1.0 - (-1.0 / (self.sample_rate as f64 * 0.035)).exp();
        let lazyness = (self.config.change_lazyness_percent / 100.0).clamp(0.0, 1.0);
        let deadband_db = lazyness * 3.0;
        let release_seconds = 0.250 + lazyness * 1.750;
        let release = 1.0 - (-1.0 / (self.sample_rate as f64 * release_seconds)).exp();
        let limiter_release = 1.0 - (-1.0 / (self.sample_rate as f64 * 0.100)).exp();
        let cross_active = self.config.enabled && self.config.xdgain_enabled;
        let desired_db = if self.config.enabled {
            20.0 * upcoming.dynamic.max(0.000001).log10()
        } else {
            0.0
        };
        if !self.envelope_started {
            // At a new stream there is no preceding audio on which to ramp.
            self.dynamic_gain = 10.0_f64.powf(desired_db / 20.0);
            if cross_active && current.audible {
                self.cross_song.offset_db = self.cross_song.offset_db.min(upcoming.offset_db);
            }
            self.held_dynamic_db = desired_db;
            self.held_offset_db = upcoming.offset_db;
            self.envelope_started = true;
        }
        // Hysteresis rejects repeated small corrections instead of chasing every
        // 10ms RMS fluctuation. Large changes still start the fast attack ahead.
        if (desired_db - self.held_dynamic_db).abs() > deadband_db {
            self.held_dynamic_db = desired_db;
        }
        if (upcoming.offset_db - self.held_offset_db).abs() > deadband_db {
            self.held_offset_db = upcoming.offset_db;
        }
        let desired_db = self.held_dynamic_db;
        let desired_offset_db = self.held_offset_db;
        let mut dynamic_db = 20.0 * self.dynamic_gain.max(0.000001).log10();
        let mut output_energy = 0.0;
        let mut output_peak = 0.0_f64;
        let mut output_samples = 0;
        for frame in bytes.chunks_exact_mut(self.channels * 4) {
            let coefficient = if desired_db < dynamic_db {
                attack
            } else {
                release
            };
            dynamic_db += (desired_db - dynamic_db) * coefficient;
            self.dynamic_gain = 10.0_f64.powf(dynamic_db / 20.0);
            let cross_gain = if cross_active && current.audible {
                let coefficient = if desired_offset_db < self.cross_song.offset_db {
                    attack
                } else {
                    release
                };
                self.cross_song.offset_db +=
                    (desired_offset_db - self.cross_song.offset_db) * coefficient;
                10.0_f64.powf(self.cross_song.offset_db / 20.0)
            } else {
                1.0
            };
            // Do not boost silence or inaudible noise, and keep volume independent.
            let gain = if self.config.enabled && current.audible {
                self.dynamic_gain * cross_gain * self.master_gain
            } else {
                self.master_gain
            };
            let safe = if current.peak * gain > OUTPUT_CEILING {
                OUTPUT_CEILING / (current.peak * gain)
            } else {
                1.0
            };
            self.limiter_gain = if safe < self.limiter_gain {
                safe
            } else {
                self.limiter_gain + (safe - self.limiter_gain) * limiter_release
            };
            for sample in frame.chunks_exact_mut(4) {
                let value = f32::from_ne_bytes(sample.try_into().unwrap()) as f64;
                let output = if value.is_finite() {
                    value * gain * self.limiter_gain
                } else {
                    0.0
                };
                let output = output.clamp(-OUTPUT_CEILING, OUTPUT_CEILING) as f32;
                output_energy += (output as f64).powi(2);
                output_peak = output_peak.max((output as f64).abs());
                output_samples += 1;
                sample.copy_from_slice(&output.to_ne_bytes());
            }
        }
        if output_samples > 0 {
            self.diagnostics.sequence = self.diagnostics.sequence.wrapping_add(1);
            self.diagnostics.input_rms = current.rms;
            self.diagnostics.input_peak = current.peak;
            self.diagnostics.output_rms = (output_energy / output_samples as f64).sqrt();
            self.diagnostics.output_peak = output_peak;
            self.diagnostics.rolling_rms = current.average;
            self.diagnostics.window_seconds = current.window_seconds;
            self.diagnostics.audible_seconds = current.audible_seconds;
            self.diagnostics.requested_gain = current.dynamic;
            self.diagnostics.preview_gain = upcoming.dynamic;
            self.diagnostics.applied_gain = if self.config.enabled && current.audible {
                self.dynamic_gain
            } else {
                1.0
            };
            self.diagnostics.xdgain_db = if cross_active && current.audible {
                self.cross_song.offset_db
            } else {
                0.0
            };
            self.diagnostics.limiter_gain = self.limiter_gain;
        }
    }

    /// Convenience for offline unit tests. Streaming playback uses the buffered
    /// element, which provides upcoming targets before rendering each block.
    #[cfg(test)]
    pub fn process(&mut self, bytes: &mut [u8]) {
        let block_bytes = (self.sample_rate / 100).max(1) * self.channels * 4;
        for block in bytes.chunks_mut(block_bytes) {
            let target = self.analyze(block);
            self.render(block, target, target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn processor(flatten_percent: f64, ra_seconds: f64) -> DynamicGain {
        let mut gain = DynamicGain::default();
        gain.set_format(1000, 2);
        gain.configure(Config {
            enabled: true,
            flatten_percent,
            ra_seconds,
            change_lazyness_percent: 0.0,
            ..Config::default()
        });
        gain
    }

    fn run(gain: &mut DynamicGain, amplitude: f32, frames: usize) -> Vec<f32> {
        let mut bytes: Vec<u8> = (0..frames)
            .flat_map(|_| [amplitude, -amplitude / 2.0])
            .flat_map(f32::to_ne_bytes)
            .collect();
        gain.process(&mut bytes);
        bytes
            .chunks_exact(4)
            .map(|b| f32::from_ne_bytes(b.try_into().unwrap()))
            .collect()
    }

    fn run_stream(gain: &mut DynamicGain, segments: &[(f32, usize)]) -> Vec<f32> {
        let block_frames = (gain.sample_rate / 100).max(1);
        let mut blocks = Vec::new();
        for &(amplitude, frames) in segments {
            for start in (0..frames).step_by(block_frames) {
                let bytes: Vec<u8> = (0..block_frames.min(frames - start))
                    .flat_map(|_| [amplitude, -amplitude / 2.0])
                    .flat_map(f32::to_ne_bytes)
                    .collect();
                let target = gain.analyze(&bytes);
                blocks.push((bytes, target));
            }
        }
        let mut output = Vec::new();
        for i in 0..blocks.len() {
            let mut upcoming = blocks[i].1;
            for (_, target) in &blocks[i..(i + 26).min(blocks.len())] {
                upcoming.dynamic = upcoming.dynamic.min(target.dynamic);
                if target.audible {
                    upcoming.offset_db = upcoming.offset_db.min(target.offset_db);
                }
            }
            let (bytes, target) = &mut blocks[i];
            gain.render(bytes, *target, upcoming);
            output.extend(
                bytes
                    .chunks_exact(4)
                    .map(|b| f32::from_ne_bytes(b.try_into().unwrap())),
            );
        }
        output
    }

    #[test]
    fn disabled_is_unity_and_master_boost_is_real() {
        let mut gain = DynamicGain::default();
        let dry = run(&mut gain, 0.1, 100);
        assert_eq!(dry[0], 0.1);
        gain.master_gain = 5.0;
        let boosted = run(&mut gain, 0.1, 100);
        assert_eq!(boosted[0], 0.5);
        gain.master_gain = 0.0;
        assert!(run(&mut gain, 0.1, 100).iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn quiet_passage_rises_and_sudden_loud_entry_is_attenuated() {
        let mut gain = processor(0.0, 15.0);
        run(&mut gain, 0.2, 5000);
        let quiet = run(&mut gain, 0.04, 2000);
        assert!(quiet[quiet.len() - 2] > 0.15);
        // Streaming supplies 250ms of upcoming audio, allowing a smooth ramp
        // during the preceding quiet passage instead of a step on the entry.
        let transition = run_stream(&mut gain, &[(0.04, 500), (0.8, 100)]);
        assert!(
            transition[500 * 2] < 0.25,
            "loud entry escaped: {}",
            transition[500 * 2]
        );
        assert!(transition[490 * 2] < transition[100 * 2] / 4.0);
    }

    #[test]
    fn tolerance_preserves_variation_and_zero_flattens() {
        let mut flat = processor(0.0, 15.0);
        let mut flexible = processor(100.0, 15.0);
        run(&mut flat, 0.2, 5000);
        run(&mut flexible, 0.2, 5000);
        let flat_transition = run_stream(&mut flat, &[(0.2, 300), (0.3, 10)]);
        assert!(flat_transition[300 * 2] < 0.21);
        assert_eq!(run(&mut flexible, 0.3, 10)[0], 0.3);
        // 100% also permits a drop to half the reference, without boosting it.
        assert!((run(&mut flexible, 0.1, 10)[0] - 0.1).abs() < 0.00001);
    }

    #[test]
    fn reference_expires_after_window_instead_of_using_song_average() {
        let mut gain = processor(0.0, 1.0);
        run(&mut gain, 0.4, 1000);
        run(&mut gain, 0.04, 1500);
        assert!((gain.dynamic_gain - 1.0).abs() < 0.001);
        assert!(gain.history_frames <= 1000);
    }

    #[test]
    fn silence_is_not_amplified_and_stereo_balance_is_preserved() {
        let mut gain = processor(0.0, 15.0);
        run(&mut gain, 0.4, 1000);
        let tiny = run(&mut gain, 0.000001, 1000);
        assert_eq!(tiny[0], 0.000001);
        assert!(
            run(&mut gain, 0.0, 1000)
                .iter()
                .all(|sample| *sample == 0.0)
        );
        let quiet = run(&mut gain, 0.1, 500);
        for frame in quiet.chunks_exact(2) {
            assert!((frame[0] + frame[1] * 2.0).abs() < 1e-6);
        }
    }

    #[test]
    fn limiter_protects_master_boost_even_with_dgain_disabled() {
        let mut gain = DynamicGain::default();
        gain.master_gain = 5.0;
        let loud = run(&mut gain, 0.8, 1000);
        assert!(
            loud.iter()
                .all(|sample| sample.is_finite() && sample.abs() <= 0.99901)
        );
    }

    #[test]
    fn reset_drops_previous_track_and_preserves_master() {
        let mut gain = processor(0.0, 15.0);
        gain.master_gain = 0.5;
        run(&mut gain, 0.4, 1000);
        run(&mut gain, 0.04, 1000);
        gain.reset();
        assert_eq!(run(&mut gain, 0.04, 10)[0], 0.02);
        assert_eq!(gain.master_gain, 0.5);
    }

    #[test]
    fn dynamic_analysis_is_independent_of_master_and_ducking() {
        let mut normal = processor(0.0, 15.0);
        let mut boosted = processor(0.0, 15.0);
        boosted.master_gain = 5.0;
        run(&mut normal, 0.04, 1000);
        run(&mut boosted, 0.04, 1000);
        let dry = run(&mut normal, 0.01, 1000);
        let loud = run(&mut boosted, 0.01, 1000);
        assert_eq!(normal.dynamic_gain, boosted.dynamic_gain);
        assert_eq!(normal.energy, boosted.energy);
        for (a, b) in dry.iter().zip(loud.iter()) {
            assert!((a * 5.0 - b).abs() < 1e-6);
        }
        let previous_gain = boosted.dynamic_gain;
        let previous_frames = boosted.history_frames;
        boosted.master_gain = 0.2;
        boosted.configure(boosted.config);
        assert_eq!(boosted.dynamic_gain, previous_gain);
        assert_eq!(boosted.history_frames, previous_frames);
        run(&mut normal, 0.02, 1000);
        run(&mut boosted, 0.02, 1000);
        assert_eq!(normal.dynamic_gain, boosted.dynamic_gain);
    }

    fn cross_song_processor() -> DynamicGain {
        let mut gain = processor(50.0, 15.0);
        gain.configure(Config {
            xdgain_enabled: true,
            ..gain.config
        });
        gain
    }

    #[test]
    fn xdgain_anchors_actual_first_audio_then_carries_offset_across_three_songs() {
        let mut gain = cross_song_processor();
        // Represents playback resumed in the middle of any playlist song.
        run(&mut gain, 0.2, 4000);
        let target = gain.cross_song.target_rms.unwrap();
        assert!((target - 0.2 * (0.625_f64).sqrt()).abs() < 1e-6);
        assert!(gain.cross_song.offset_db.abs() < 1e-5);
        gain.reset();
        let quiet = run(&mut gain, 0.02, 8000);
        assert!((quiet[quiet.len() - 2] - 0.2).abs() < 0.001);
        let inherited_offset = gain.cross_song.offset_db;
        assert!((inherited_offset - 20.0).abs() < 0.01);
        gain.reset();
        assert_eq!(gain.cross_song.offset_db, inherited_offset);
        assert_eq!(gain.cross_song.target_rms, Some(target));
        // The third song inherits the offset, then reduces it before a louder entry.
        let third = run(&mut gain, 0.05, 4000);
        assert!(third.iter().all(|sample| sample.abs() <= 0.201));
        assert!((third[third.len() - 2] - 0.2).abs() < 0.001);
        assert_eq!(gain.cross_song.target_rms, Some(target));
    }

    #[test]
    fn xdgain_does_not_anchor_silence_and_short_first_song_still_sets_reference() {
        let mut gain = cross_song_processor();
        run(&mut gain, 0.0, 5000);
        run(&mut gain, 0.000001, 5000);
        assert_eq!(gain.cross_song.target_rms, None);
        run(&mut gain, 0.1, 500);
        assert_eq!(gain.cross_song.target_rms, None);
        let carried = gain.cross_song_state();
        assert!(carried.target_rms.is_some());
        let mut next = cross_song_processor();
        next.restore_cross_song_state(carried);
        run(&mut next, 0.01, 8000);
        let before_silence = next.cross_song;
        let silence = run(&mut next, 0.0, 20000);
        assert!(silence.iter().all(|sample| *sample == 0.0));
        assert_eq!(next.cross_song, before_silence);
        assert_eq!(next.cross_song.target_rms, carried.target_rms);
    }

    #[test]
    fn xdgain_preserves_variation_and_is_independent_of_master_volume() {
        let mut normal = cross_song_processor();
        let mut boosted = cross_song_processor();
        boosted.master_gain = 5.0;
        run(&mut normal, 0.04, 4000);
        run(&mut boosted, 0.04, 4000);
        normal.reset();
        boosted.reset();
        let quiet = run(&mut normal, 0.01, 8000);
        let loud = run(&mut boosted, 0.01, 8000);
        assert_eq!(normal.cross_song, boosted.cross_song);
        assert_eq!(normal.master_gain, 1.0);
        for (a, b) in quiet.iter().zip(loud.iter()) {
            assert!((a * 5.0 - b).abs() < 1e-6);
        }
        // Variation within dGain's 50% band remains, rather than flattening all samples.
        let variation = run(&mut normal, 0.012, 10);
        assert!(variation[0] > 0.046);
        let before_format_change = normal.cross_song;
        normal.set_format(2000, 1);
        assert_eq!(normal.cross_song, before_format_change);
    }

    #[test]
    fn xdgain_requires_dgain_and_toggle_off_starts_a_new_reference() {
        let mut gain = cross_song_processor();
        run(&mut gain, 0.2, 4000);
        gain.reset();
        run(&mut gain, 0.02, 8000);
        let carried = gain.cross_song;
        gain.configure(Config {
            enabled: false,
            ..gain.config
        });
        assert_eq!(run(&mut gain, 0.02, 100)[0], 0.02);
        assert_eq!(gain.cross_song, carried);
        gain.configure(Config {
            enabled: true,
            ..gain.config
        });
        assert_eq!(gain.cross_song, carried);
        gain.configure(Config {
            xdgain_enabled: false,
            ..gain.config
        });
        assert_eq!(gain.cross_song, CrossSongGain::default());
        gain.configure(Config {
            xdgain_enabled: true,
            ..gain.config
        });
        run(&mut gain, 0.05, 4000);
        assert!((gain.cross_song.target_rms.unwrap() - 0.05 * 0.625_f64.sqrt()).abs() < 1e-6);
    }

    #[test]
    fn xdgain_offset_is_bounded_and_combined_boost_cannot_clip() {
        let mut gain = cross_song_processor();
        run(&mut gain, 0.5, 4000);
        gain.reset();
        gain.master_gain = 5.0;
        let quiet = run(&mut gain, 0.0002, 10000);
        assert!(gain.cross_song.offset_db <= XDGAIN_MAX_DB);
        let loud = run(&mut gain, 0.9, 1000);
        assert!(
            quiet
                .iter()
                .chain(loud.iter())
                .all(|sample| sample.is_finite() && sample.abs() <= 0.99901)
        );
    }
    #[test]
    fn lookahead_smooths_before_loud_entry_and_preserves_all_audio_frames() {
        let mut gain = processor(0.0, 20.0);
        let output = run_stream(&mut gain, &[(0.1, 1000), (0.6, 100), (0.1, 777)]);
        assert_eq!(output.len(), 1877 * 2);
        assert!(output[990 * 2] < output[700 * 2] / 3.0);
        assert!(output[1000 * 2] < 0.15);
        // No instantaneous 10ms gain step in the preceding constant passage.
        for frame in (751..1000).map(|i| i * 2) {
            assert!((output[frame] - output[frame - 2]).abs() < 0.005);
        }
    }

    #[test]
    fn lazyness_rejects_small_repeated_gain_changes_and_endpoints_are_finite() {
        fn modulation(lazyness: f64) -> f64 {
            let mut gain = processor(0.0, 20.0);
            gain.configure(Config {
                change_lazyness_percent: lazyness,
                ..gain.config
            });
            let baseline = GainTarget {
                rms: 0.1,
                average: 0.1,
                window_seconds: 1.0,
                audible_seconds: 1.0,
                dynamic: 1.0,
                offset_db: 0.0,
                audible: true,
                peak: 0.1,
            };
            let mut bytes = vec![0u8; 80];
            gain.render(&mut bytes, baseline, baseline);
            let mut total = 0.0;
            let mut previous = gain.dynamic_gain;
            for i in 0..1000 {
                let target = GainTarget {
                    dynamic: if (i / 30) % 2 == 0 { 0.9 } else { 1.1 },
                    ..baseline
                };
                gain.render(&mut bytes, baseline, target);
                assert!(gain.dynamic_gain.is_finite());
                total += (gain.dynamic_gain - previous).abs();
                previous = gain.dynamic_gain;
            }
            total
        }
        let reactive = modulation(0.0);
        let lazy = modulation(100.0);
        assert!(reactive > 1.0, "zero lazyness should respond: {reactive}");
        assert!(lazy < reactive * 0.1, "lazy {lazy}, reactive {reactive}");
    }

    #[test]
    fn maximum_lazyness_still_anticipates_large_entries_and_protects_clipping() {
        let mut gain = processor(0.0, 20.0);
        gain.configure(Config {
            change_lazyness_percent: 100.0,
            ..gain.config
        });
        let output = run_stream(&mut gain, &[(0.05, 1000), (0.9, 500)]);
        assert!(output[1000 * 2] < 0.12);
        gain.master_gain = 5.0;
        assert!(
            run(&mut gain, 0.9, 200)
                .iter()
                .all(|s| s.is_finite() && s.abs() <= 0.99901)
        );
    }
}

#[cfg(test)]
mod diagnostics_tests {
    use super::*;
    fn pcm(level: f32, samples: usize) -> Vec<u8> {
        (0..samples).flat_map(|_| level.to_ne_bytes()).collect()
    }
    #[test]
    fn diagnostics_match_original_block_not_future_analysis_and_measured_output() {
        let mut gain = DynamicGain::default();
        gain.configure(Config {
            enabled: true,
            flatten_percent: 0.0,
            ..Config::default()
        });
        let mut quiet = pcm(0.1, 960);
        let current = gain.analyze(&quiet);
        let loud = gain.analyze(&pcm(0.9, 960));
        gain.render(&mut quiet, current, loud);
        gain.set_diagnostic_position(Some(4200));
        let frame = gain.diagnostics();
        assert!((frame.input_rms - 0.1).abs() < 1e-6);
        assert!((frame.rolling_rms - 0.1).abs() < 1e-6);
        assert!((frame.window_seconds - 0.01).abs() < 1e-9);
        assert_eq!(frame.requested_gain, current.dynamic);
        assert_eq!(frame.preview_gain, loud.dynamic);
        assert_eq!(frame.position_millis, Some(4200));
        let values: Vec<f64> = quiet
            .chunks_exact(4)
            .map(|v| f32::from_ne_bytes(v.try_into().unwrap()) as f64)
            .collect();
        let rms = (values.iter().map(|v| v * v).sum::<f64>() / values.len() as f64).sqrt();
        assert!((frame.output_rms - rms).abs() < 1e-10);
        assert!((frame.output_peak - values.iter().copied().fold(0.0, f64::max)).abs() < 1e-10);
        assert_eq!(frame.sequence, 1);
        assert!(serde_json::to_string(&frame).is_ok());
    }
    #[test]
    fn disabled_dgain_still_reports_master_and_limiter_and_resets_cleanly() {
        let mut gain = DynamicGain::default();
        gain.master_gain = 5.0;
        let mut bytes = pcm(0.8, 960);
        let target = gain.analyze(&bytes);
        gain.render(&mut bytes, target, target);
        let frame = gain.diagnostics();
        assert_eq!(frame.applied_gain, 1.0);
        assert_eq!(frame.master_gain, 5.0);
        assert!(frame.limiter_gain < 0.3);
        assert!(frame.output_peak <= 1.0);
        assert_eq!(frame.rolling_rms, 0.0);
        let generation = frame.generation;
        gain.configure(Config {
            enabled: true,
            ..Config::default()
        });
        assert!(gain.diagnostics().generation > generation);
        assert_eq!(gain.diagnostics().sequence, 0);
        assert_eq!(gain.diagnostics().output_rms, 0.0);
        assert!(gain.diagnostics().settings.enabled);
        assert_eq!(gain.diagnostics().master_gain, 5.0);
        let next = gain.diagnostics().generation;
        gain.reset();
        assert!(gain.diagnostics().generation > next);
    }
}
