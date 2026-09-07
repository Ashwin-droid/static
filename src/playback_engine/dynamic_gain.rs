//! Linked-channel gain processing. All timing is measured in decoded audio frames.
use std::collections::VecDeque;

const SILENCE_RMS: f64 = 0.0001;
const MAX_DYNAMIC_GAIN: f64 = 5.0;
const OUTPUT_CEILING: f64 = 0.999;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Config {
    pub enabled: bool,
    pub flatten_percent: f64,
    pub ra_seconds: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            flatten_percent: 50.0,
            ra_seconds: 15.0,
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
            master_gain: 1.0,
        }
    }
}

impl DynamicGain {
    pub fn configure(&mut self, config: Config) {
        if self.config != config {
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
            let desired = if self.config.enabled {
                let average = self.observe(frames, rms * rms);
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
            let largest_gain = self.dynamic_gain.max(desired) * self.master_gain;
            let safe = if peak * largest_gain > OUTPUT_CEILING {
                OUTPUT_CEILING / (peak * largest_gain)
            } else {
                1.0
            };
            self.limiter_gain = self.limiter_gain.min(safe);
            for frame in block[..frames * frame_bytes].chunks_exact_mut(frame_bytes) {
                self.dynamic_gain += (desired - self.dynamic_gain) * recovery;
                self.limiter_gain += (safe - self.limiter_gain) * limiter_recovery;
                let gain = self.dynamic_gain * self.master_gain * self.limiter_gain;
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
}
