//! Bounded post-gain audio observation. Only the request path performs FFT work.
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use rustfft::{Fft, FftPlanner, num_complex::Complex};

use crate::ipc::VisualizationFrame;

const FFT_SIZE: usize = 4096;
const HALF_SIZE: usize = FFT_SIZE / 2;
const CAPTURE_IDLE_MS: u64 = 2000;
const STALE_AFTER: Duration = Duration::from_millis(500);
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

struct Ring {
    generation: u64,
    sequence: u64,
    sample_rate: u32,
    samples: [f32; FFT_SIZE],
    power: [f32; FFT_SIZE],
    peaks: [f32; FFT_SIZE],
    cursor: usize,
    len: usize,
    updated: Option<Instant>,
}

impl Default for Ring {
    fn default() -> Self {
        Self {
            generation: 0,
            sequence: 0,
            sample_rate: 48_000,
            samples: [0.0; FFT_SIZE],
            power: [0.0; FFT_SIZE],
            peaks: [0.0; FFT_SIZE],
            cursor: 0,
            len: 0,
            updated: None,
        }
    }
}

impl Ring {
    fn sync_generation(&mut self, generation: u64) {
        if self.generation != generation {
            self.generation = generation;
            self.len = 0;
            self.cursor = 0;
            self.updated = None;
        }
    }
}

pub(super) struct Spectrum {
    started: Instant,
    requested_ms: AtomicU64,
    generation: AtomicU64,
    ring: Mutex<Ring>,
    analyzer: Mutex<Option<Analyzer>>,
}

impl Default for Spectrum {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            requested_ms: AtomicU64::new(u64::MAX),
            generation: AtomicU64::new(NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)),
            ring: Mutex::new(Ring::default()),
            analyzer: Mutex::new(None),
        }
    }
}

impl Spectrum {
    /// Safe on the streaming thread, including when a request holds the ring lock.
    pub fn reset(&self) {
        self.generation.store(
            NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            Ordering::Release,
        );
    }

    /// Capture the newest bounded portion of interleaved native-endian float PCM.
    /// Never wait for readers and never allocate or perform spectral analysis here.
    pub fn capture(&self, bytes: &[u8], sample_rate: u32, channels: usize) {
        let requested = self.requested_ms.load(Ordering::Relaxed);
        let now_ms = self.started.elapsed().as_millis() as u64;
        if requested == u64::MAX || now_ms.saturating_sub(requested) > CAPTURE_IDLE_MS {
            return;
        }
        let Some(frame_bytes) = channels.checked_mul(4).filter(|n| *n > 0) else {
            return;
        };
        let Ok(mut ring) = self.ring.try_lock() else {
            return;
        };
        ring.sync_generation(self.generation.load(Ordering::Acquire));
        ring.sample_rate = sample_rate.max(1);
        let frames = bytes.len() / frame_bytes;
        let skip = frames.saturating_sub(FFT_SIZE) * frame_bytes;
        for frame in bytes[skip..].chunks_exact(frame_bytes) {
            let mut sum = 0.0;
            let mut power = 0.0;
            let mut peak = 0.0_f32;
            for bytes in frame.chunks_exact(4) {
                let sample = f32::from_ne_bytes(bytes.try_into().unwrap());
                let sample = if sample.is_finite() {
                    sample.clamp(-1.0, 1.0)
                } else {
                    0.0
                };
                sum += sample;
                power += sample * sample;
                peak = peak.max(sample.abs());
            }
            let cursor = ring.cursor;
            ring.samples[cursor] = sum / channels as f32;
            ring.power[cursor] = power / channels as f32;
            ring.peaks[cursor] = peak;
            ring.cursor = (cursor + 1) % FFT_SIZE;
            ring.len = (ring.len + 1).min(FFT_SIZE);
        }
        if frames > 0 {
            ring.sequence = ring.sequence.wrapping_add(1);
            ring.updated = Some(Instant::now());
        }
    }

    pub fn frame(&self, bands: usize, playing: bool) -> VisualizationFrame {
        let bands = bands.clamp(8, 128);
        let now_ms = self.started.elapsed().as_millis() as u64;
        let previous = self.requested_ms.swap(now_ms, Ordering::Relaxed);
        if previous != u64::MAX && now_ms.saturating_sub(previous) > CAPTURE_IDLE_MS {
            self.reset();
        }
        // Copy a fixed amount under the ring lock, then release it before the FFT.
        let (generation, sequence, sample_rate, samples, rms, peak, audible) = {
            let mut ring = self.ring.lock().unwrap();
            ring.sync_generation(self.generation.load(Ordering::Acquire));
            let audible =
                playing && ring.updated.is_some_and(|t| t.elapsed() < STALE_AFTER) && ring.len > 0;
            let mut samples = [0.0; FFT_SIZE];
            let mut power = 0.0;
            let mut peak = 0.0_f32;
            if audible {
                for i in 0..ring.len {
                    let index = (ring.cursor + FFT_SIZE - ring.len + i) % FFT_SIZE;
                    samples[FFT_SIZE - ring.len + i] = ring.samples[index];
                    power += ring.power[index];
                    peak = peak.max(ring.peaks[index]);
                }
            }
            (
                ring.generation,
                ring.sequence,
                ring.sample_rate,
                samples,
                if audible {
                    (power / ring.len as f32).sqrt()
                } else {
                    0.0
                },
                peak,
                audible,
            )
        };
        let mut analyzer = self.analyzer.lock().unwrap();
        let analyzer = analyzer.get_or_insert_with(Analyzer::new);
        if analyzer.generation != generation || !audible {
            analyzer.generation = generation;
            analyzer.sequence = None;
            analyzer.smoothed.fill(0.0);
        }
        if audible && analyzer.sequence != Some(sequence) {
            analyzer.analyze(&samples);
            analyzer.sequence = Some(sequence);
        }
        VisualizationFrame {
            generation,
            sequence,
            sample_rate,
            bands: if audible {
                analyzer.bands(bands, sample_rate)
            } else {
                vec![0.0; bands]
            },
            rms: rms.clamp(0.0, 1.0),
            peak: peak.clamp(0.0, 1.0),
        }
    }
}

struct Analyzer {
    fft: std::sync::Arc<dyn Fft<f32>>,
    scratch: Vec<Complex<f32>>,
    input: Vec<Complex<f32>>,
    window: [f32; FFT_SIZE],
    smoothed: [f32; HALF_SIZE],
    generation: u64,
    sequence: Option<u64>,
}

impl Analyzer {
    fn new() -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_SIZE);
        let scratch = vec![Complex::default(); fft.get_inplace_scratch_len()];
        Self {
            fft,
            scratch,
            input: vec![Complex::default(); FFT_SIZE],
            // WebAudio uses a Blackman window and smoothing of FFT magnitudes.
            window: std::array::from_fn(|i| {
                let phase = std::f32::consts::TAU * i as f32 / FFT_SIZE as f32;
                0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos()
            }),
            smoothed: [0.0; HALF_SIZE],
            generation: 0,
            sequence: None,
        }
    }

    fn analyze(&mut self, samples: &[f32; FFT_SIZE]) {
        for (i, sample) in samples.iter().enumerate() {
            self.input[i] = Complex::new(sample * self.window[i], 0.0);
        }
        self.fft
            .process_with_scratch(&mut self.input, &mut self.scratch);
        for (i, smoothed) in self.smoothed.iter_mut().enumerate() {
            let magnitude = self.input[i].norm() / FFT_SIZE as f32;
            *smoothed = 0.8 * *smoothed + 0.2 * magnitude;
        }
    }

    fn bands(&self, count: usize, sample_rate: u32) -> Vec<f32> {
        let nyquist = sample_rate as f32 / 2.0;
        let low = 20.0_f32.min(nyquist / 2.0);
        (0..count)
            .map(|i| {
                let start_hz = low * (nyquist / low).powf(i as f32 / count as f32);
                let end_hz = low * (nyquist / low).powf((i + 1) as f32 / count as f32);
                let start = ((start_hz * FFT_SIZE as f32 / sample_rate as f32) as usize)
                    .clamp(1, HALF_SIZE - 1);
                let end = ((end_hz * FFT_SIZE as f32 / sample_rate as f32).ceil() as usize)
                    .clamp(start + 1, HALF_SIZE);
                let magnitude = self.smoothed[start..end]
                    .iter()
                    .copied()
                    .fold(0.0_f32, f32::max);
                ((20.0 * magnitude.max(1e-10).log10() + 100.0) / 70.0).clamp(0.0, 1.0)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(amplitude: f32, frequency: f32) -> Vec<u8> {
        (0..FFT_SIZE)
            .flat_map(|i| {
                (amplitude * (std::f32::consts::TAU * frequency * i as f32 / 48_000.0).sin())
                    .to_ne_bytes()
            })
            .collect()
    }

    #[test]
    fn synthetic_sine_lands_in_its_logarithmic_frequency_band() {
        let spectrum = Spectrum::default();
        spectrum.frame(64, true);
        let frequency = 48_000.0 * 85.0 / FFT_SIZE as f32;
        spectrum.capture(&sine(0.1, frequency), 48_000, 1);
        let frame = spectrum.frame(64, true);
        let strongest = frame
            .bands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        let expected = ((frequency / 20.0).ln() / (24_000.0_f32 / 20.0).ln() * 64.0) as usize;
        assert_eq!(strongest, expected);
        assert!((frame.rms - 0.1 / 2.0_f32.sqrt()).abs() < 0.0001);
        assert!((frame.peak - 0.1).abs() < 0.0001);
    }

    #[test]
    fn reset_pause_and_replacement_never_return_old_audio() {
        let spectrum = Spectrum::default();
        spectrum.frame(32, true);
        spectrum.capture(&sine(0.1, 1000.0), 48_000, 1);
        let frame = spectrum.frame(32, true);
        assert!(frame.peak > 0.0);
        assert!(spectrum.frame(32, false).bands.iter().all(|v| *v == 0.0));
        spectrum.reset();
        let reset = spectrum.frame(32, true);
        assert_ne!(reset.generation, frame.generation);
        assert_eq!(reset.peak, 0.0);
        assert!(reset.bands.iter().all(|v| *v == 0.0));
        assert_ne!(
            Spectrum::default().frame(32, true).generation,
            reset.generation
        );
    }

    #[test]
    fn band_count_invalid_pcm_and_capture_limits_are_bounded() {
        let spectrum = Spectrum::default();
        spectrum.capture(&sine(0.1, 1000.0), 48_000, 1);
        assert_eq!(
            spectrum.ring.lock().unwrap().len,
            0,
            "no capture without subscriber"
        );
        assert_eq!(spectrum.frame(0, true).bands.len(), 8);
        spectrum.capture(&f32::NAN.to_ne_bytes().repeat(FFT_SIZE * 3), 48_000, 1);
        spectrum.capture(&[], 48_000, usize::MAX);
        let frame = spectrum.frame(usize::MAX, true);
        assert_eq!(spectrum.ring.lock().unwrap().len, FFT_SIZE);
        assert_eq!(frame.bands.len(), 128);
        assert!(
            frame
                .bands
                .iter()
                .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
        );
        assert_eq!(frame.rms, 0.0);
        let _held = spectrum.ring.lock().unwrap();
        spectrum.capture(&sine(0.1, 1000.0), 48_000, 1);
        spectrum.reset(); // Neither streaming operation waits for the reader.
    }

    #[test]
    fn levels_follow_processed_master_volume_without_rescaling_the_fft() {
        use super::super::dynamic_gain::DynamicGain;
        let observe = |master_gain: f64| {
            let mut gain = DynamicGain::default();
            gain.set_format(48_000, 1);
            gain.master_gain = master_gain;
            let mut pcm = sine(0.001, 48_000.0 * 85.0 / FFT_SIZE as f32);
            gain.process(&mut pcm);
            let spectrum = Spectrum::default();
            spectrum.frame(64, true);
            spectrum.capture(&pcm, 48_000, 1);
            spectrum.frame(64, true)
        };
        let unity = observe(1.0);
        let boost = observe(5.0);
        assert!((boost.rms / unity.rms - 5.0).abs() < 0.0001);
        assert!((boost.peak / unity.peak - 5.0).abs() < 0.0001);
        let band = unity
            .bands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        let expected_change = 20.0 * 5.0_f32.log10() / 70.0;
        assert!((boost.bands[band] - unity.bands[band] - expected_change).abs() < 0.0001);
    }

    #[test]
    fn anti_phase_stereo_preserves_rms_and_peak_metering() {
        let spectrum = Spectrum::default();
        spectrum.frame(32, true);
        let pcm: Vec<u8> = (0..FFT_SIZE)
            .flat_map(|_| [0.2_f32, -0.2_f32].into_iter().flat_map(f32::to_ne_bytes))
            .collect();
        spectrum.capture(&pcm, 48_000, 2);
        let frame = spectrum.frame(32, true);
        assert!((frame.rms - 0.2).abs() < 0.0001);
        assert_eq!(frame.peak, 0.2);
        assert!(frame.bands.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn subscriber_timeout_stops_capture_and_stale_frames_are_silent() {
        let mut spectrum = Spectrum::default();
        spectrum.frame(32, true);
        spectrum.capture(&sine(0.1, 1000.0), 48_000, 1);
        spectrum.ring.lock().unwrap().updated = Some(Instant::now() - STALE_AFTER);
        assert_eq!(spectrum.frame(32, true).peak, 0.0);
        spectrum.started = Instant::now() - Duration::from_secs(3);
        let sequence = spectrum.ring.lock().unwrap().sequence;
        spectrum.capture(&sine(0.1, 1000.0), 48_000, 1);
        assert_eq!(spectrum.ring.lock().unwrap().sequence, sequence);
        assert_eq!(spectrum.frame(32, true).peak, 0.0);
    }
}
