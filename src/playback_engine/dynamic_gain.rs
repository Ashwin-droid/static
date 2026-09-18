//! Linked-channel gain processing. All timing is measured in decoded audio frames.

use super::gain_history::GainHistory;

pub(super) const OPENING_GUARD_NS: u64 = 15_000_000_000;
const SILENCE_RMS: f64 = 0.0001;
const MAX_DYNAMIC_GAIN: f64 = 5.0;
const OUTPUT_CEILING: f64 = 0.999;

/// Session state, carried into replacement engines independently of song history.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct CrossSongGain {
    last_wra_db: Option<f64>,
}

pub(super) use crate::ipc::GainCalibrationSettings as Config;
use crate::ipc::GainDiagnostics;
use std::sync::atomic::{AtomicU64, Ordering};
static DIAGNOSTIC_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Gain intent computed from original audio, before either gain stage.
#[derive(Clone, Copy, Debug)]
pub(super) struct GainTarget {
    pub opening_guard_millis: u64,
    pub dynamic: f64,
    pub audible: bool,
    pub peak: f64,
    pub rms: f64,
    pub average: f64,
    pub window_seconds: f64,
    pub audible_seconds: f64,
}

pub(super) struct DynamicGain {
    history: GainHistory,
    next_position_ns: u64,
    needs_restore: bool,
    config: Config,
    diagnostics: GainDiagnostics,
    sample_rate: usize,
    channels: usize,
    weighted_db: Option<f64>,
    song_reference_db: Option<f64>,
    last_output_wra_db: Option<f64>,
    observed_frames: usize,
    audible_frames: usize,
    dynamic_gain: f64,
    limiter_gain: f64,
    cross_song: CrossSongGain,
    pub master_gain: f64,
    revision: u64,
    stream_revision: u64,
    envelope_started: bool,
}

fn db_to_rms(db: f64) -> f64 {
    10.0_f64.powf(db / 20.0)
}

impl Default for DynamicGain {
    fn default() -> Self {
        Self {
            history: GainHistory::default(),
            next_position_ns: 0,
            needs_restore: true,
            config: Config::default(),
            diagnostics: GainDiagnostics {
                generation: DIAGNOSTIC_GENERATION.fetch_add(1, Ordering::Relaxed),
                ..Default::default()
            },
            sample_rate: 48_000,
            channels: 2,
            weighted_db: None,
            song_reference_db: None,
            last_output_wra_db: None,
            observed_frames: 0,
            audible_frames: 0,
            dynamic_gain: 1.0,
            limiter_gain: 1.0,
            cross_song: CrossSongGain::default(),
            master_gain: 1.0,
            revision: 0,
            stream_revision: 0,
            envelope_started: false,
        }
    }
}

impl DynamicGain {
    pub fn configure(&mut self, config: Config) {
        if self.config != config {
            if self.config.xdgain_enabled != config.xdgain_enabled {
                self.cross_song = CrossSongGain::default();
                self.song_reference_db = None;
                self.last_output_wra_db = None;
            }
            self.config = config;
            // Edits change future evolution, not measurements already attached to
            // this song's timeline. Reanalyze queued PCM without discarding it.
            self.history.finish_span();
            self.revision = self.revision.wrapping_add(1);
        }
    }

    pub fn set_format(&mut self, sample_rate: usize, channels: usize) {
        if self.sample_rate != sample_rate || self.channels != channels {
            let new_rate = sample_rate.max(1);
            self.observed_frames = (self.observed_frames as u128 * new_rate as u128
                / self.sample_rate as u128)
                .min(usize::MAX as u128) as usize;
            self.audible_frames = (self.audible_frames as u128 * new_rate as u128
                / self.sample_rate as u128)
                .min(usize::MAX as u128) as usize;
            self.sample_rate = new_rate;
            self.channels = channels.max(1);
            self.reset();
        }
    }

    /// Flush transport/envelopes while retaining this song's WRA timeline.
    pub fn reset(&mut self) {
        self.history.finish_span();
        self.stream_revision = self.stream_revision.wrapping_add(1);
        self.revision = self.revision.wrapping_add(1);
        self.needs_restore = true;
        self.envelope_started = false;
        self.dynamic_gain = 1.0;
        self.limiter_gain = 1.0;
    }

    pub fn seek(&mut self, position_ns: u64) {
        self.reset();
        self.next_position_ns = position_ns;
        self.weighted_db = self
            .history
            .at(position_ns)
            .or_else(|| self.history.before(position_ns))
            .unwrap_or(self.song_reference_db);
    }

    fn reset_history(&mut self) {
        self.history.clear();
        self.needs_restore = true;
        self.diagnostics = GainDiagnostics {
            generation: DIAGNOSTIC_GENERATION.fetch_add(1, Ordering::Relaxed),
            ..Default::default()
        };
        self.revision = self.revision.wrapping_add(1);
        self.envelope_started = false;
        self.weighted_db = self
            .song_reference_db
            .filter(|_| self.config.xdgain_enabled);
        self.observed_frames = 0;
        self.audible_frames = 0;
        self.dynamic_gain = 1.0;
        self.limiter_gain = 1.0;
    }

    /// Commit only the last WRA emitted for this song, never decoded lookahead.
    /// Consuming the candidate makes EOS, stop and replacement idempotent.
    pub fn finish_song(&mut self) {
        if let Some(last) = self.last_output_wra_db.take()
            && self.config.enabled
            && self.config.xdgain_enabled
        {
            self.cross_song.last_wra_db = Some(last);
        }
    }

    /// A song boundary is explicit; flush/seek/caps resets never advance the chain.
    pub fn begin_song(&mut self) {
        self.song_reference_db = if self.config.xdgain_enabled {
            self.cross_song.last_wra_db
        } else {
            None
        };
        self.last_output_wra_db = None;
        self.next_position_ns = 0;
        self.reset_history();
        self.reset();
    }

    pub fn cross_song_state(&mut self) -> CrossSongGain {
        self.finish_song();
        self.cross_song
    }

    pub fn restore_cross_song_state(&mut self, state: CrossSongGain) {
        self.cross_song = state;
        self.begin_song();
    }

    /// All weighted terms are in dBFS. Lazyness continuously sets input/history
    /// weights. Chained songs reserve a fixed 20% for the previous-song reference.
    /// Fractional-time weights compose exactly across sample rates/block sizes.
    fn observe(&mut self, frames: usize, rms: f64) -> f64 {
        self.observed_frames = self.observed_frames.saturating_add(frames);
        if frames == 0 || rms < SILENCE_RMS {
            return self.weighted_db.map_or(0.0, db_to_rms);
        }
        self.audible_frames = self.audible_frames.saturating_add(frames);
        let input_db = 20.0 * rms.log10();
        let anchor = self
            .song_reference_db
            .filter(|_| self.config.xdgain_enabled);
        let previous = self.weighted_db.unwrap_or(anchor.unwrap_or(input_db));
        let lazyness = (self.config.change_lazyness_percent / 100.0).clamp(0.0, 1.0);
        // History's share moves linearly from 10% to 90%. A chained reference
        // always reserves 20%; input/history split the remaining 80%.
        let recursive_share = 0.1 + 0.8 * lazyness;
        let (input_weight, reference_weight, history_weight) = if anchor.is_some() {
            (0.8 * (1.0 - recursive_share), 0.2, 0.8 * recursive_share)
        } else {
            (1.0 - recursive_share, 0.0, recursive_share)
        };
        let destination = (input_weight * input_db + reference_weight * anchor.unwrap_or(0.0))
            / (1.0 - history_weight);
        let seconds = frames as f64 / self.sample_rate as f64;
        let fresh_weight = -(history_weight.ln() * seconds / self.config.ra_seconds).exp_m1();
        let average_db = previous + (destination - previous) * fresh_weight;
        self.weighted_db = Some(average_db);
        db_to_rms(average_db)
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

    pub fn analysis_position_ns(&self) -> u64 {
        self.next_position_ns
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Analyze each original audio block exactly once, as it enters lookahead.
    /// The rolling reference advances in audio time independently of master gain.
    // Algorithm tests start after the intro; transport/guard tests supply real PTS.
    #[cfg(test)]
    pub fn analyze(&mut self, bytes: &[u8]) -> GainTarget {
        let position = self.next_position_ns.max(OPENING_GUARD_NS);
        self.analyze_at(bytes, Some(position))
    }

    pub fn analyze_at(&mut self, bytes: &[u8], position_ns: Option<u64>) -> GainTarget {
        let start = position_ns.unwrap_or(self.next_position_ns);
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
        let duration = (frames as u128 * 1_000_000_000 / self.sample_rate as u128) as u64;
        let end = start.saturating_add(duration);
        if self.needs_restore || start.abs_diff(self.next_position_ns) > 1_000 {
            self.history.finish_span();
            self.weighted_db = self
                .history
                .at(start)
                .or_else(|| self.history.before(start))
                .unwrap_or(self.song_reference_db);
            if start < OPENING_GUARD_NS {
                self.weighted_db = self.song_reference_db;
            }
            self.history.start_span(start, self.weighted_db);
            self.needs_restore = false;
        }
        self.next_position_ns = end;
        let average = if self.config.enabled && frames > 0 {
            if end <= OPENING_GUARD_NS {
                // The opening cannot establish or drag down a WRA reference.
                self.weighted_db = self.song_reference_db;
                self.observed_frames = self.observed_frames.saturating_add(frames);
            } else if let Some(recorded) = self.history.at(end) {
                self.weighted_db = recorded;
                self.observed_frames = self.observed_frames.saturating_add(frames);
            } else {
                let eligible_start = start.max(OPENING_GUARD_NS);
                let eligible_frames = if start >= OPENING_GUARD_NS {
                    frames
                } else {
                    ((end - eligible_start) as u128 * self.sample_rate as u128 / 1_000_000_000)
                        as usize
                }
                .min(frames);
                let observed_rms = if eligible_frames < frames && eligible_frames > 0 {
                    let tail = &bytes[(frames - eligible_frames) * self.channels * 4..];
                    let energy: f64 = tail
                        .chunks_exact(4)
                        .map(|v| {
                            let v = f32::from_ne_bytes(v.try_into().unwrap()) as f64;
                            if v.is_finite() { v * v } else { 0.0 }
                        })
                        .sum();
                    (energy / (eligible_frames * self.channels) as f64).sqrt()
                } else {
                    rms
                };
                self.observe(eligible_frames, observed_rms);
            }
            self.history.record(end, self.weighted_db);
            self.weighted_db.map_or(0.0, db_to_rms)
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
        GainTarget {
            opening_guard_millis: OPENING_GUARD_NS.saturating_sub(start) / 1_000_000,
            dynamic,
            audible,
            peak,
            rms,
            average,
            window_seconds: self.observed_frames as f64 / self.sample_rate as f64,
            audible_seconds: self.audible_frames as f64 / self.sample_rate as f64,
        }
    }

    pub fn diagnostics(&self) -> GainDiagnostics {
        GainDiagnostics {
            settings: self.config,
            master_gain: self.master_gain,
            xdgain_reference_dbfs: self.song_reference_db,
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
        // Linear in dB: scale correction against the WRA toward unity.
        // At zero strength correction is naturally 0 dB while the settings,
        // rolling analysis and cross-song reference remain enabled.
        let correction_strength = 1.0 - lazyness;
        // Keep smoothing independent of strength so the control remains linear.
        let release = 1.0 - (-1.0 / (self.sample_rate as f64 * 0.250)).exp();
        let limiter_release = 1.0 - (-1.0 / (self.sample_rate as f64 * 0.100)).exp();
        let desired_db = if self.config.enabled {
            20.0 * upcoming.dynamic.max(0.000001).log10()
        } else {
            0.0
        };
        if !self.envelope_started {
            // At a new stream there is no preceding audio on which to ramp.
            self.dynamic_gain = 10.0_f64.powf(desired_db / 20.0);
            self.envelope_started = true;
        }
        let mut dynamic_db = 20.0 * self.dynamic_gain.max(0.000001).log10();
        let mut gain_output_energy = 0.0;
        let mut gain_output_peak = 0.0_f64;
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
            // Do not boost silence or inaudible noise, and keep volume independent.
            let song_gain = if self.config.enabled && current.audible {
                self.dynamic_gain.powf(correction_strength)
            } else {
                1.0
            };
            let gain = song_gain * self.master_gain;
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
                // Measure before volume/limiting, including when master is muted.
                // Dividing final output by master would fail at zero or clipping.
                let gain_output = if value.is_finite() {
                    value * song_gain
                } else {
                    0.0
                };
                gain_output_energy += gain_output * gain_output;
                gain_output_peak = gain_output_peak.max(gain_output.abs());
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
            self.diagnostics.opening_guard_millis = current.opening_guard_millis;
            self.diagnostics.input_rms = current.rms;
            self.diagnostics.input_peak = current.peak;
            self.diagnostics.gain_output_rms = (gain_output_energy / output_samples as f64).sqrt();
            self.diagnostics.gain_output_peak = gain_output_peak;
            self.diagnostics.output_rms = (output_energy / output_samples as f64).sqrt();
            self.diagnostics.output_peak = output_peak;
            self.diagnostics.rolling_rms = current.average;
            self.diagnostics.window_seconds = current.window_seconds;
            self.diagnostics.audible_seconds = current.audible_seconds;
            self.diagnostics.requested_gain = current.dynamic.powf(correction_strength);
            self.diagnostics.preview_gain = upcoming.dynamic.powf(correction_strength);
            self.diagnostics.applied_gain = if self.config.enabled && current.audible {
                self.dynamic_gain.powf(correction_strength)
            } else {
                1.0
            };
            if self.config.enabled && current.audible {
                self.last_output_wra_db = Some(20.0 * current.average.log10());
            }
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
        // At 0% laziness, WRA retains 10% per interval (2/15 of one interval here).
        let expected = 0.04 * 5.0_f64.powf(0.1_f64.powf(2.0 / 15.0));
        assert!((quiet[quiet.len() - 2] as f64 - expected).abs() < 0.01);
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
    fn weighted_reference_retains_history_beyond_the_old_window() {
        let mut gain = processor(0.0, 1.0);
        gain.configure(Config {
            change_lazyness_percent: 25.0,
            ..gain.config
        });
        gain.observe(1000, 0.4);
        let after_one = gain.observe(1000, 0.04);
        assert!(
            (20.0 * after_one.log10()
                - (20.0 * 0.04_f64.log10() * 0.7 + 20.0 * 0.4_f64.log10() * 0.3))
                .abs()
                < 1e-12
        );
        let after_two = gain.observe(1000, 0.04);
        assert!(
            (20.0 * after_two.log10()
                - (20.0 * 0.04_f64.log10() * 0.7 + 20.0 * after_one.log10() * 0.3))
                .abs()
                < 1e-12
        );
        assert!(after_two > 0.04);
        gain.observe(1000, 0.0);
        assert!((db_to_rms(gain.weighted_db.unwrap()) - after_two).abs() < 1e-12);
    }

    #[test]
    fn weighted_average_is_independent_of_sample_rate_and_block_size() {
        for rate in [1000, 44100, 48000] {
            for chunk in [1, rate / 100, rate / 4] {
                let mut gain = processor(0.0, 2.0);
                gain.configure(Config {
                    change_lazyness_percent: 25.0,
                    ..gain.config
                });
                gain.set_format(rate, 2);
                gain.observe(rate, 0.1);
                let mut frames = rate * 2;
                while frames > 0 {
                    let count = frames.min(chunk);
                    gain.observe(count, 0.5);
                    frames -= count;
                }
                assert!(
                    (gain.weighted_db.unwrap() - (0.7 * 20.0 * 0.5_f64.log10() - 6.0)).abs()
                        < 1e-10
                );
            }
        }
    }

    #[test]
    fn gradually_louder_input_is_corrected_against_its_weighted_history() {
        let mut gain = processor(20.0, 30.0);
        run(&mut gain, 0.05, 15000);
        for step in 0..6000 {
            let amplitude = 0.05 + 0.45 * (step + 1) as f32 / 6000.0;
            run(&mut gain, amplitude, 10);
        }
        let frame = gain.diagnostics();
        assert!(frame.rolling_rms * 1.2 < frame.input_rms);
        assert!(frame.requested_gain < 1.0);
        assert!(frame.gain_output_rms < frame.input_rms);
        assert!(frame.applied_gain < 1.0);
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
        gain.begin_song();
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
        assert_eq!(normal.weighted_db, boosted.weighted_db);
        for (a, b) in dry.iter().zip(loud.iter()) {
            assert!((a * 5.0 - b).abs() < 1e-6);
        }
        let previous_gain = boosted.dynamic_gain;
        let previous_frames = boosted.observed_frames;
        boosted.master_gain = 0.2;
        boosted.configure(boosted.config);
        assert_eq!(boosted.dynamic_gain, previous_gain);
        assert_eq!(boosted.observed_frames, previous_frames);
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

    fn emit_db(gain: &mut DynamicGain, db: f64, frames: usize) -> f64 {
        let sample = db_to_rms(db) as f32;
        let mut bytes: Vec<u8> = (0..frames * gain.channels)
            .flat_map(|_| sample.to_ne_bytes())
            .collect();
        let target = gain.analyze(&bytes);
        gain.render(&mut bytes, target, target);
        20.0 * target.average.log10()
    }

    #[test]
    fn xdgain_chains_the_final_wra_across_three_songs_in_dbfs() {
        let mut gain = cross_song_processor();
        gain.configure(Config {
            ra_seconds: 1.0,
            change_lazyness_percent: 34.375,
            ..gain.config
        });
        emit_db(&mut gain, -30.0, 5000);
        assert_eq!(gain.diagnostics().xdgain_reference_dbfs, None);
        assert_eq!(gain.cross_song.last_wra_db, None);
        let first_end = emit_db(&mut gain, -10.0, 1000);
        assert!((first_end - -17.5).abs() < 1e-5);
        assert_eq!(gain.diagnostics().xdgain_reference_dbfs, None);
        gain.finish_song();
        // Finalizing cannot change the current song's fixed anchor.
        assert_eq!(gain.song_reference_db, None);
        gain.begin_song();
        assert!((gain.song_reference_db.unwrap() - first_end).abs() < 1e-10);
        let second_start = emit_db(&mut gain, -40.0, 1000);
        assert!((second_start - -28.75).abs() < 1e-5);
        let second_end = emit_db(&mut gain, -5.0, 1000);
        assert!((second_end - -14.625).abs() < 1e-5);
        assert!((gain.song_reference_db.unwrap() - first_end).abs() < 1e-10);
        gain.finish_song();
        gain.begin_song();
        assert!((gain.song_reference_db.unwrap() - second_end).abs() < 1e-10);
        assert!((emit_db(&mut gain, -20.0, 1000) - -17.3125).abs() < 1e-5);
    }

    #[test]
    fn final_reference_uses_emitted_audio_not_unplayed_lookahead() {
        let mut gain = cross_song_processor();
        gain.configure(Config {
            ra_seconds: 1.0,
            ..gain.config
        });
        let heard = emit_db(&mut gain, -30.0, 1000);
        let future: Vec<u8> = (0..2000).flat_map(|_| 1.0_f32.to_ne_bytes()).collect();
        gain.analyze(&future);
        assert!(gain.weighted_db.unwrap() > heard + 10.0);
        let state = gain.cross_song_state();
        assert!((state.last_wra_db.unwrap() - heard).abs() < 1e-10);
        gain.reset();
        gain.finish_song();
        assert_eq!(gain.cross_song, state);
        let mut next = cross_song_processor();
        next.restore_cross_song_state(state);
        assert_eq!(next.song_reference_db, state.last_wra_db);
    }

    #[test]
    fn silence_empty_songs_and_repeated_boundaries_do_not_replace_the_reference() {
        let mut gain = cross_song_processor();
        emit_db(&mut gain, -120.0, 1000);
        gain.finish_song();
        gain.begin_song();
        assert_eq!(gain.song_reference_db, None);
        let audible = emit_db(&mut gain, -20.0, 100);
        emit_db(&mut gain, -120.0, 1000);
        gain.finish_song();
        let state = gain.cross_song;
        gain.finish_song();
        gain.reset();
        gain.begin_song();
        gain.finish_song();
        assert_eq!(gain.cross_song, state);
        assert!((state.last_wra_db.unwrap() - audible).abs() < 1e-10);
    }

    #[test]
    fn seeks_format_and_setting_changes_keep_the_current_fixed_reference() {
        let mut gain = cross_song_processor();
        emit_db(&mut gain, -20.0, 1000);
        gain.finish_song();
        gain.begin_song();
        let reference = gain.song_reference_db;
        emit_db(&mut gain, -40.0, 1000);
        let last = gain.last_output_wra_db;
        gain.reset();
        gain.set_format(2000, 1);
        gain.configure(Config {
            flatten_percent: 20.0,
            ..gain.config
        });
        assert_eq!(gain.song_reference_db, reference);
        assert_eq!(gain.cross_song.last_wra_db, reference);
        // No post-seek audio: the last heard WRA is still the handoff candidate.
        gain.finish_song();
        assert_eq!(gain.cross_song.last_wra_db, last);
    }

    #[test]
    fn disabling_xdgain_clears_the_reference_but_keeps_the_recorded_wra() {
        let mut gain = cross_song_processor();
        emit_db(&mut gain, -10.0, 1000);
        gain.finish_song();
        gain.begin_song();
        emit_db(&mut gain, -30.0, 1000);
        let previous = gain.weighted_db.unwrap();
        gain.configure(Config {
            xdgain_enabled: false,
            ..gain.config
        });
        assert_eq!(gain.song_reference_db, None);
        assert_eq!(gain.cross_song, CrossSongGain::default());
        let level = emit_db(&mut gain, -30.0, 1000);
        let expected = -30.0 + (previous + 30.0) * 0.1_f64.powf(1.0 / 15.0);
        assert!((level - expected).abs() < 1e-5);
    }

    #[test]
    fn chained_weighting_composes_over_partial_intervals() {
        for rate in [1000, 44100, 48000] {
            for chunk in [rate / 100, rate / 4, rate] {
                let mut gain = cross_song_processor();
                gain.configure(Config {
                    ra_seconds: 2.0,
                    change_lazyness_percent: 34.375,
                    ..gain.config
                });
                gain.set_format(rate, 2);
                gain.restore_cross_song_state(CrossSongGain {
                    last_wra_db: Some(-20.0),
                });
                for start in (0..rate * 2).step_by(chunk) {
                    gain.observe(chunk.min(rate * 2 - start), 0.001);
                }
                // 0.5*(-60)+0.2*(-20)+0.3*(-20) = -40 dBFS.
                assert!((gain.weighted_db.unwrap() - -40.0).abs() < 1e-9);
                assert_eq!(gain.song_reference_db, Some(-20.0));
            }
        }
    }

    #[test]
    fn chained_gain_is_independent_of_master_and_cannot_clip_final_output() {
        let mut gain = cross_song_processor();
        emit_db(&mut gain, -10.0, 1000);
        gain.finish_song();
        gain.begin_song();
        gain.master_gain = 5.0;
        let quiet = run(&mut gain, 0.0002, 1000);
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
    fn lazyness_scales_small_repeated_corrections_and_endpoints_are_finite() {
        fn modulation(lazyness: f64) -> f64 {
            let mut gain = processor(0.0, 20.0);
            gain.configure(Config {
                change_lazyness_percent: lazyness,
                ..gain.config
            });
            let baseline = GainTarget {
                opening_guard_millis: 0,
                rms: 0.1,
                average: 0.1,
                window_seconds: 1.0,
                audible_seconds: 1.0,
                dynamic: 1.0,
                audible: true,
                peak: 0.1,
            };
            let mut bytes = vec![0u8; 80];
            gain.render(&mut bytes, baseline, baseline);
            let mut total = 0.0;
            let mut previous = gain.diagnostics().applied_gain;
            for i in 0..1000 {
                let target = GainTarget {
                    dynamic: if (i / 30) % 2 == 0 { 0.9 } else { 1.1 },
                    ..baseline
                };
                gain.render(&mut bytes, baseline, target);
                assert!(gain.dynamic_gain.is_finite());
                total += (gain.diagnostics().applied_gain - previous).abs();
                previous = gain.diagnostics().applied_gain;
            }
            total
        }
        let reactive = modulation(0.0);
        let lazy = modulation(100.0);
        assert!(reactive > 1.0, "zero lazyness should respond: {reactive}");
        assert!(lazy < reactive * 0.1, "lazy {lazy}, reactive {reactive}");
    }

    #[test]
    fn correction_against_chained_wra_scales_linearly_without_disabling_settings() {
        // A settled target isolates the strength curve from attack/release time.
        for dynamic in [0.5, 2.0] {
            for percent in [0.0, 25.0, 50.0, 75.0, 99.0, 99.9, 100.0] {
                let mut gain = cross_song_processor();
                gain.configure(Config {
                    change_lazyness_percent: percent,
                    ..gain.config
                });
                let reference = -20.0;
                gain.restore_cross_song_state(CrossSongGain {
                    last_wra_db: Some(reference),
                });
                let mut bytes: Vec<u8> = (0..20).flat_map(|_| 0.1f32.to_ne_bytes()).collect();
                let mut target = gain.analyze(&bytes);
                target.dynamic = dynamic;
                gain.render(&mut bytes, target, target);
                let strength = 1.0 - percent / 100.0;
                let frame = gain.diagnostics();
                let dynamic_db = 20.0 * dynamic.log10();
                assert!((20.0 * frame.applied_gain.log10() - dynamic_db * strength).abs() < 1e-8);
                assert!(
                    (20.0 * (frame.gain_output_rms / frame.input_rms).log10()
                        - dynamic_db * strength)
                        .abs()
                        < 1e-6
                );
                assert!(gain.config.enabled && gain.config.xdgain_enabled);
                assert_eq!(gain.song_reference_db, Some(reference));
            }
        }
    }

    #[test]
    fn lazyness_smoothly_weights_wra_with_a_fixed_xdgain_share() {
        for percent in [0.0, 12.5, 25.0, 50.0, 75.0, 99.9, 100.0] {
            for chained in [false, true] {
                let mut gain = cross_song_processor();
                gain.configure(Config {
                    ra_seconds: 1.0,
                    change_lazyness_percent: percent,
                    ..gain.config
                });
                if chained {
                    gain.restore_cross_song_state(CrossSongGain {
                        last_wra_db: Some(-20.0),
                    });
                }
                gain.weighted_db = Some(-30.0);
                let output = 20.0 * gain.observe(1000, db_to_rms(-10.0)).log10();
                let h = 0.1 + 0.8 * percent / 100.0;
                let expected = if chained {
                    -10.0 * 0.8 * (1.0 - h) - 20.0 * 0.2 - 30.0 * 0.8 * h
                } else {
                    -10.0 * (1.0 - h) - 30.0 * h
                };
                assert!((output - expected).abs() < 1e-10);
                if percent == 0.0 {
                    assert!((output - if chained { -13.6 } else { -12.0 }).abs() < 1e-10);
                }
                if percent == 100.0 {
                    assert!((output - if chained { -26.4 } else { -28.0 }).abs() < 1e-10);
                }
            }
        }
    }

    #[test]
    fn lowering_lazyness_restores_correction_using_the_same_xdgain_reference() {
        let mut gain = cross_song_processor();
        emit_db(&mut gain, -10.0, 1000);
        gain.finish_song();
        gain.begin_song();
        let reference = gain.song_reference_db;
        emit_db(&mut gain, -30.0, 100);
        assert!(gain.diagnostics().applied_gain > 1.0);
        gain.configure(Config {
            change_lazyness_percent: 100.0,
            ..gain.config
        });
        emit_db(&mut gain, -30.0, 100);
        assert_eq!(gain.diagnostics().applied_gain, 1.0);
        assert_eq!(gain.song_reference_db, reference);
        gain.configure(Config {
            change_lazyness_percent: 0.0,
            ..gain.config
        });
        emit_db(&mut gain, -30.0, 100);
        assert!(gain.diagnostics().applied_gain > 1.0);
        assert_eq!(gain.song_reference_db, reference);
        assert!(gain.config.enabled && gain.config.xdgain_enabled);
    }

    #[test]
    fn maximum_lazyness_applies_unity_but_keeps_master_and_clipping_protection() {
        let mut gain = processor(0.0, 20.0);
        gain.configure(Config {
            change_lazyness_percent: 100.0,
            ..gain.config
        });
        let output = run_stream(&mut gain, &[(0.05, 1000), (0.9, 500)]);
        assert!((output[1000 * 2] - 0.9).abs() < 1e-6);
        assert_eq!(gain.diagnostics().applied_gain, 1.0);
        assert_eq!(gain.diagnostics().requested_gain, 1.0);
        assert_eq!(gain.diagnostics().preview_gain, 1.0);
        assert!(gain.config.enabled);
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
        assert!((frame.requested_gain - current.dynamic.powf(0.5)).abs() < 1e-12);
        assert_eq!(frame.preview_gain, loud.dynamic.powf(0.5));
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
    fn calibration_output_is_independent_of_master_even_when_muted_or_limited() {
        let mut gain = DynamicGain::default();
        gain.configure(Config {
            enabled: true,
            xdgain_enabled: true,
            ..Config::default()
        });
        gain.restore_cross_song_state(CrossSongGain {
            last_wra_db: Some(20.0 * 0.8_f64.log10()),
        });
        let mut observed = Vec::new();
        for master in [1.0, 0.5, 0.0, 5.0] {
            gain.master_gain = master;
            let mut bytes = pcm(0.8, 960);
            let target = gain.analyze(&bytes);
            gain.render(&mut bytes, target, target);
            observed.push(gain.diagnostics());
        }
        for frame in &observed {
            assert!((frame.gain_output_rms - observed[0].gain_output_rms).abs() < 1e-6);
            assert!((frame.gain_output_peak - observed[0].gain_output_peak).abs() < 1e-6);
            assert!((frame.rolling_rms - observed[0].rolling_rms).abs() < 1e-6);
            assert!((frame.applied_gain - observed[0].applied_gain).abs() < 1e-6);
        }
        assert!((observed[1].output_rms / observed[0].output_rms - 0.5).abs() < 1e-6);
        assert_eq!(observed[2].output_rms, 0.0);
        assert!(observed[2].gain_output_rms > 0.79);
        assert!(observed[3].limiter_gain < 0.3);
        assert!(observed[3].output_peak <= 1.0);
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
        assert_eq!(gain.diagnostics().generation, generation);
        assert_eq!(gain.diagnostics().sequence, frame.sequence);
        assert_eq!(gain.diagnostics().output_rms, frame.output_rms);
        assert!(gain.diagnostics().settings.enabled);
        assert_eq!(gain.diagnostics().master_gain, 5.0);
        let next = gain.diagnostics().generation;
        gain.reset();
        assert_eq!(gain.diagnostics().generation, next);
    }
}

#[cfg(test)]
mod seek_guard_tests {
    use super::*;
    fn processor() -> DynamicGain {
        let mut gain = DynamicGain::default();
        gain.set_format(1000, 1);
        gain.configure(Config {
            enabled: true,
            flatten_percent: 0.0,
            change_lazyness_percent: 0.0,
            ..Config::default()
        });
        gain
    }
    fn block(gain: &mut DynamicGain, ms: u64, amplitude: f32) -> GainDiagnostics {
        let mut bytes: Vec<u8> = (0..10).flat_map(|_| amplitude.to_ne_bytes()).collect();
        let target = gain.analyze_at(&bytes, Some(ms * 1_000_000));
        gain.render(&mut bytes, target, target);
        gain.set_diagnostic_position(Some(ms));
        gain.diagnostics()
    }
    #[test]
    fn opening_never_learns_quiet_input_and_no_reference_is_passthrough() {
        let mut gain = processor();
        for ms in (0..15000).step_by(10) {
            let frame = block(&mut gain, ms, 0.0005);
            assert_eq!(frame.rolling_rms, 0.0);
            assert_eq!(frame.applied_gain, 1.0);
            assert!((frame.gain_output_rms - frame.input_rms).abs() < 1e-12);
        }
        assert_eq!(gain.weighted_db, None);
        let frame = block(&mut gain, 15000, 0.2);
        assert!((frame.rolling_rms - 0.2).abs() < 1e-6);
        assert_eq!(frame.opening_guard_millis, 0);
    }
    #[test]
    fn chained_reference_is_held_for_opening_and_learning_begins_at_15_seconds() {
        let mut gain = processor();
        gain.configure(Config {
            xdgain_enabled: true,
            ..gain.config
        });
        gain.restore_cross_song_state(CrossSongGain {
            last_wra_db: Some(-20.0),
        });
        for ms in (0..15000).step_by(10) {
            assert!((block(&mut gain, ms, 0.001).rolling_rms - 0.1).abs() < 1e-9);
        }
        let before = gain.weighted_db.unwrap();
        block(&mut gain, 15000, 0.001);
        assert!(gain.weighted_db.unwrap() < before);
        assert_eq!(gain.song_reference_db, Some(-20.0));
    }
    #[test]
    fn seeking_reuses_the_song_timeline_without_restarting_average_or_graph() {
        let mut gain = processor();
        for ms in (15000..18000).step_by(10) {
            block(&mut gain, ms, if ms < 16000 { 0.2 } else { 0.02 });
        }
        gain.history.finish_span();
        let expected = gain.history.at(16_510_000_000).unwrap().unwrap();
        let later = gain.history.at(17_510_000_000).unwrap().unwrap();
        let generation = gain.diagnostics().generation;
        let sequence = gain.diagnostics().sequence;
        for _ in 0..3 {
            gain.seek(16_500_000_000);
            gain.reset();
            gain.reset(); // GStreamer flush + segment
            let frame = block(&mut gain, 16500, 0.02);
            assert!((20.0 * frame.rolling_rms.log10() - expected).abs() < 1e-8);
            assert_eq!(frame.generation, generation);
            assert!(frame.sequence > sequence);
            gain.seek(17_500_000_000);
            let frame = block(&mut gain, 17500, 0.02);
            assert!((20.0 * frame.rolling_rms.log10() - later).abs() < 1e-8);
        }
        gain.seek(5_000_000_000);
        let intro = block(&mut gain, 5000, 0.0005);
        assert_eq!(intro.applied_gain, 1.0);
        assert_eq!(intro.rolling_rms, 0.0);
        gain.seek(17_500_000_000);
        assert!((20.0 * block(&mut gain, 17500, 0.02).rolling_rms.log10() - later).abs() < 1e-8);
    }
    #[test]
    fn unvisited_seek_keeps_earlier_reference_and_does_not_start_a_new_guard() {
        let mut gain = processor();
        block(&mut gain, 15000, 0.2);
        gain.seek(60_000_000_000);
        gain.reset();
        let frame = block(&mut gain, 60000, 0.02);
        assert_eq!(frame.opening_guard_millis, 0);
        assert!(frame.rolling_rms > 0.19);
        let generation = frame.generation;
        gain.begin_song();
        assert!(gain.diagnostics().generation > generation);
        assert_eq!(block(&mut gain, 0, 0.001).rolling_rms, 0.0);
    }
}
