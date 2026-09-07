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

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Config {
    pub enabled: bool,
    pub flatten_percent: f64,
    pub ra_seconds: f64,
    pub xdgain_enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            flatten_percent: 50.0,
            ra_seconds: 15.0,
            xdgain_enabled: false,
        }
    }
}

#[derive(Debug)]
struct Observation {
    frames: usize,
    // Silence occupies time in the window, but is not a loudness reference.
    audible: bool,
    mean_square: f64,
}

pub(super) struct DynamicGain {
    config: Config,
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
}

impl Default for DynamicGain {
    fn default() -> Self {
        Self {
            config: Config::default(),
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
            self.reset();
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

    /// Process native-endian, interleaved f32 audio without requiring aligned buffers.
    /// A short block's peak is known before any samples are amplified, preventing a
    /// sudden loud entry from escaping the limiter. No playback lookahead is added.
    pub fn process(&mut self, bytes: &mut [u8]) {
        let frame_bytes = self.channels * 4;
        let block_bytes = (self.sample_rate / 100).max(1) * frame_bytes;
        let recovery = 1.0 - (-1.0 / (self.sample_rate as f64 * 0.250)).exp();
        let limiter_recovery = 1.0 - (-1.0 / (self.sample_rate as f64 * 0.100)).exp();
        let cross_song_recovery = 1.0 - (-1.0 / self.sample_rate as f64).exp();
        for block in bytes.chunks_mut(block_bytes) {
            let frames = block.len() / frame_bytes;
            if frames == 0 {
                continue;
            }
            let mut square_sum = 0.0;
            let mut peak = 0.0_f64;
            for sample in block[..frames * frame_bytes].chunks_exact(4) {
                let value = f32::from_ne_bytes(sample.try_into().unwrap()) as f64;
                let value = if value.is_finite() { value } else { 0.0 };
                square_sum += value * value;
                peak = peak.max(value.abs());
            }
            let rms = (square_sum / (frames * self.channels) as f64).sqrt();
            let average = if self.config.enabled {
                self.observe(frames, rms * rms)
            } else {
                0.0
            };
            let desired = if self.config.enabled {
                if rms < SILENCE_RMS || average < SILENCE_RMS {
                    1.0
                } else {
                    let tolerance = 1.0 + self.config.flatten_percent / 100.0;
                    let target = rms.clamp(average / tolerance, average * tolerance);
                    (target / rms).clamp(0.0, MAX_DYNAMIC_GAIN)
                }
            } else {
                1.0
            };
            // Attenuate before the first sample of a loud block; gain recovers
            // gradually to avoid pumping and zipper noise in quiet passages.
            self.dynamic_gain = self.dynamic_gain.min(desired);
            let cross_song_active = self.config.enabled && self.config.xdgain_enabled;
            let cross_song_audible = cross_song_active && rms >= SILENCE_RMS;
            let desired_offset = if cross_song_active {
                self.desired_cross_song_offset(rms, average, frames)
            } else {
                self.cross_song.offset_db
            };
            if cross_song_audible {
                // Do not carry a quiet song's boost into a loud entry unchecked.
                self.cross_song.offset_db = self.cross_song.offset_db.min(desired_offset);
            }
            let largest_cross_song_gain = if cross_song_audible {
                10.0_f64.powf(desired_offset.max(self.cross_song.offset_db) / 20.0)
            } else {
                1.0
            };
            let largest_gain =
                self.dynamic_gain.max(desired) * largest_cross_song_gain * self.master_gain;
            let safe = if peak * largest_gain > OUTPUT_CEILING {
                OUTPUT_CEILING / (peak * largest_gain)
            } else {
                1.0
            };
            self.limiter_gain = self.limiter_gain.min(safe);
            for frame in block[..frames * frame_bytes].chunks_exact_mut(frame_bytes) {
                self.dynamic_gain += (desired - self.dynamic_gain) * recovery;
                self.limiter_gain += (safe - self.limiter_gain) * limiter_recovery;
                let cross_song_gain = if cross_song_audible {
                    self.cross_song.offset_db +=
                        (desired_offset - self.cross_song.offset_db) * cross_song_recovery;
                    10.0_f64.powf(self.cross_song.offset_db / 20.0)
                } else {
                    1.0
                };
                let gain =
                    self.dynamic_gain * cross_song_gain * self.master_gain * self.limiter_gain;
                for sample in frame.chunks_exact_mut(4) {
                    let value = f32::from_ne_bytes(sample.try_into().unwrap()) as f64;
                    let output = if value.is_finite() { value * gain } else { 0.0 };
                    sample.copy_from_slice(&(output.clamp(-1.0, 1.0) as f32).to_ne_bytes());
                }
            }
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
        let loud = run(&mut gain, 0.8, 10);
        assert!(loud[0] < 0.25, "loud first sample escaped: {}", loud[0]);
    }

    #[test]
    fn tolerance_preserves_variation_and_zero_flattens() {
        let mut flat = processor(0.0, 15.0);
        let mut flexible = processor(100.0, 15.0);
        run(&mut flat, 0.2, 5000);
        run(&mut flexible, 0.2, 5000);
        assert!(run(&mut flat, 0.3, 10)[0] < 0.21);
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
}
