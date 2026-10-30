//! Spectral band isolation: play only the frequency slices the visualiser lit.
//!
//! The band edges are the visualiser's own (`spectrum::Analyzer::bands`): the
//! same 4096-point FFT, the same log spacing from 20 Hz to Nyquist, so a lit
//! tube plays exactly the slice it draws. Processing is a 75%-overlap STFT with
//! a square-root Hann window on both sides; the mask eases between selections
//! and the wet signal crossfades with the dry one when isolation starts or
//! stops, so toggling never clicks. Output lags input by `FFT_SIZE - HOP`
//! samples (64 ms at 48 kHz) while a selection is active.
use std::collections::VecDeque;
use std::sync::Arc;

use rustfft::{Fft, FftPlanner, num_complex::Complex};

use crate::ipc::BandIsolation;

pub const FFT_SIZE: usize = 4096;
const HALF_SIZE: usize = FFT_SIZE / 2;
const HOP: usize = FFT_SIZE / 4;
/// Σ of a periodic Hann window over four quarter-hop shifts.
const OVERLAP_GAIN: f32 = 2.0;
const BLEND_MS: f32 = 120.0;
const MASK_EASE: f32 = 0.35;

struct Channel {
    input: Vec<f32>,
    cursor: usize,
    since_hop: usize,
    overlap: Vec<f32>,
    output: VecDeque<f32>,
}

impl Channel {
    fn new() -> Self {
        Self {
            input: vec![0.0; FFT_SIZE],
            cursor: 0,
            since_hop: 0,
            overlap: vec![0.0; FFT_SIZE],
            output: vec![0.0; FFT_SIZE - HOP].into(),
        }
    }
}

pub struct BandIsolator {
    selection: Option<BandIsolation>,
    rate: u32,
    channels: Vec<Channel>,
    target: Vec<f32>,
    mask: Vec<f32>,
    /// 0 = dry only, 1 = isolated only.
    blend: f32,
    fft: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex<f32>>,
    frame: Vec<Complex<f32>>,
    window: Vec<f32>,
    bytes_scratch: Vec<f32>,
}

impl Default for BandIsolator {
    fn default() -> Self {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let inverse = planner.plan_fft_inverse(FFT_SIZE);
        let scratch_len = fft
            .get_inplace_scratch_len()
            .max(inverse.get_inplace_scratch_len());
        Self {
            selection: None,
            rate: 0,
            channels: Vec::new(),
            target: vec![0.0; HALF_SIZE + 1],
            mask: vec![0.0; HALF_SIZE + 1],
            blend: 0.0,
            fft,
            inverse,
            scratch: vec![Complex::default(); scratch_len],
            frame: vec![Complex::default(); FFT_SIZE],
            bytes_scratch: Vec::new(),
            window: (0..FFT_SIZE)
                .map(|i| {
                    let hann =
                        0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT_SIZE as f32).cos();
                    hann.sqrt()
                })
                .collect(),
        }
    }
}

/// Bin range `[start, end)` of one visualiser band; mirrors `spectrum::Analyzer::bands`.
pub fn band_bins(index: usize, count: usize, sample_rate: u32) -> (usize, usize) {
    let nyquist = sample_rate as f32 / 2.0;
    let low = 20.0_f32.min(nyquist / 2.0);
    let start_hz = low * (nyquist / low).powf(index as f32 / count as f32);
    let end_hz = low * (nyquist / low).powf((index + 1) as f32 / count as f32);
    let start =
        ((start_hz * FFT_SIZE as f32 / sample_rate as f32) as usize).clamp(1, HALF_SIZE - 1);
    let end = ((end_hz * FFT_SIZE as f32 / sample_rate as f32).ceil() as usize)
        .clamp(start + 1, HALF_SIZE);
    (start, end)
}

impl BandIsolator {
    pub fn selection(&self) -> Option<BandIsolation> {
        self.selection.clone()
    }

    pub fn set_selection(&mut self, selection: Option<BandIsolation>) {
        let selection = selection.filter(|s| !s.selected.is_empty());
        if selection == self.selection {
            return;
        }
        self.selection = selection;
        self.rebuild_target();
    }

    /// A seek or new segment: forget buffered audio, keep the selection.
    pub fn reset_stream(&mut self) {
        for channel in &mut self.channels {
            *channel = Channel::new();
        }
    }

    pub fn active(&self) -> bool {
        self.selection.is_some() || self.blend > 0.0
    }

    fn rebuild_target(&mut self) {
        self.target.fill(0.0);
        let Some(selection) = &self.selection else {
            return;
        };
        if self.rate == 0 {
            return;
        }
        let count = selection.bands as usize;
        for &band in &selection.selected {
            if (band as usize) < count {
                let (start, end) = band_bins(band as usize, count, self.rate);
                self.target[start..end].fill(1.0);
            }
        }
    }

    fn ensure_format(&mut self, rate: u32, channels: usize) {
        if self.rate != rate {
            self.rate = rate;
            self.rebuild_target();
            self.mask.copy_from_slice(&self.target);
        }
        if self.channels.len() != channels {
            self.channels = (0..channels).map(|_| Channel::new()).collect();
        }
    }

    /// Native-endian interleaved F32 PCM bytes in place; a no-op while nothing is lit.
    pub fn process_bytes(&mut self, bytes: &mut [u8], rate: u32, channels: usize) {
        if !self.active() {
            self.channels.clear();
            return;
        }
        let mut samples = std::mem::take(&mut self.bytes_scratch);
        samples.clear();
        samples.extend(
            bytes
                .chunks_exact(4)
                .map(|b| f32::from_ne_bytes(b.try_into().unwrap())),
        );
        self.process(&mut samples, rate, channels);
        for (chunk, sample) in bytes.chunks_exact_mut(4).zip(&samples) {
            chunk.copy_from_slice(&sample.to_ne_bytes());
        }
        self.bytes_scratch = samples;
    }

    /// Interleaved float frames in place. Cheap no-op while nothing is lit.
    pub fn process(&mut self, samples: &mut [f32], rate: u32, channels: usize) {
        if channels == 0 || rate == 0 {
            return;
        }
        if !self.active() {
            if !self.channels.is_empty() {
                self.channels.clear();
            }
            return;
        }
        self.ensure_format(rate, channels);
        let step = 1000.0 / (BLEND_MS * rate as f32);
        let wanted = if self.selection.is_some() { 1.0 } else { 0.0 };
        for frame in samples.chunks_exact_mut(channels) {
            self.blend = if wanted > self.blend {
                (self.blend + step).min(1.0)
            } else {
                (self.blend - step).max(0.0)
            };
            for (index, dry) in frame.iter_mut().enumerate() {
                let wet = self.push(index, *dry);
                *dry += (wet - *dry) * self.blend;
            }
        }
    }

    fn push(&mut self, channel: usize, sample: f32) -> f32 {
        let state = &mut self.channels[channel];
        state.input[state.cursor] = sample;
        state.cursor = (state.cursor + 1) % FFT_SIZE;
        state.since_hop += 1;
        if state.since_hop == HOP {
            state.since_hop = 0;
            if channel == 0 {
                for (mask, target) in self.mask.iter_mut().zip(&self.target) {
                    *mask += (*target - *mask) * MASK_EASE;
                }
            }
            self.hop(channel);
        }
        self.channels[channel].output.pop_front().unwrap_or(0.0)
    }

    fn hop(&mut self, channel: usize) {
        let state = &mut self.channels[channel];
        for i in 0..FFT_SIZE {
            let sample = state.input[(state.cursor + i) % FFT_SIZE];
            self.frame[i] = Complex::new(sample * self.window[i], 0.0);
        }
        self.fft
            .process_with_scratch(&mut self.frame, &mut self.scratch);
        for k in 0..=HALF_SIZE {
            let mask = self.mask[k];
            self.frame[k] *= mask;
            if k != 0 && k != HALF_SIZE {
                self.frame[FFT_SIZE - k] *= mask;
            }
        }
        self.inverse
            .process_with_scratch(&mut self.frame, &mut self.scratch);
        let scale = 1.0 / (FFT_SIZE as f32 * OVERLAP_GAIN);
        for i in 0..FFT_SIZE {
            state.overlap[i] += self.frame[i].re * self.window[i] * scale;
        }
        state.output.extend(state.overlap[..HOP].iter().copied());
        state.overlap.copy_within(HOP.., 0);
        state.overlap[FFT_SIZE - HOP..].fill(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frequency: f32, frames: usize, rate: u32) -> Vec<f32> {
        (0..frames)
            .map(|i| (std::f32::consts::TAU * frequency * i as f32 / rate as f32).sin())
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    fn band_containing(hz: f32, count: usize, rate: u32) -> u32 {
        let bin = (hz * FFT_SIZE as f32 / rate as f32) as usize;
        (0..count)
            .find(|&i| {
                let (start, end) = band_bins(i, count, rate);
                (start..end).contains(&bin)
            })
            .unwrap() as u32
    }

    #[test]
    fn band_edges_match_the_analyzer() {
        // 64 bands: the lowest one is a sliver above 20 Hz, the top one ends at Nyquist.
        let (start, _) = band_bins(0, 64, 48_000);
        assert_eq!(start, 1);
        let (_, end) = band_bins(63, 64, 48_000);
        assert_eq!(end, HALF_SIZE);
        for i in 1..64 {
            assert!(band_bins(i - 1, 64, 48_000).1 <= band_bins(i, 64, 48_000).0 + 1);
        }
    }

    #[test]
    fn lit_band_passes_and_dark_band_is_removed() {
        let rate = 48_000;
        let mut isolator = BandIsolator::default();
        let keep = band_containing(1_000.0, 64, rate);
        isolator.set_selection(Some(BandIsolation {
            bands: 64,
            selected: vec![keep],
        }));
        let frames = rate as usize * 2;
        let mut signal: Vec<f32> = tone(1_000.0, frames, rate)
            .iter()
            .zip(tone(6_000.0, frames, rate))
            .map(|(a, b)| 0.5 * a + 0.5 * b)
            .collect();
        isolator.process(&mut signal, rate, 1);
        // Judge the settled second half against the pure 1 kHz tone, delayed by the STFT.
        let half = frames / 2;
        let delay = FFT_SIZE - HOP;
        let reference = tone(1_000.0, frames, rate);
        let residual: Vec<f32> = (half..frames)
            .map(|i| signal[i] - 0.5 * reference[i - delay])
            .collect();
        assert!(rms(&signal[half..]) > 0.3, "lit band should keep its level");
        assert!(
            rms(&residual) < 0.03,
            "dark band should be gone, residual {}",
            rms(&residual)
        );
    }

    #[test]
    fn empty_selection_is_a_bypass_and_release_fades_back_to_dry() {
        let rate = 48_000;
        let mut isolator = BandIsolator::default();
        let mut signal = tone(440.0, 4_800, rate);
        let original = signal.clone();
        isolator.process(&mut signal, rate, 1);
        assert_eq!(signal, original);
        isolator.set_selection(Some(BandIsolation {
            bands: 32,
            selected: vec![],
        }));
        assert!(isolator.selection().is_none());
        isolator.set_selection(Some(BandIsolation {
            bands: 32,
            selected: vec![31],
        }));
        let mut muted = tone(440.0, rate as usize, rate);
        isolator.process(&mut muted, rate, 1);
        assert!(rms(&muted[rate as usize / 2..]) < 0.05);
        isolator.set_selection(None);
        let mut restored = tone(440.0, rate as usize, rate);
        let dry = restored.clone();
        isolator.process(&mut restored, rate, 1);
        assert_eq!(&restored[rate as usize / 2..], &dry[rate as usize / 2..]);
        assert!(!isolator.active());
    }

    #[test]
    fn stereo_channels_are_filtered_independently() {
        let rate = 44_100;
        let mut isolator = BandIsolator::default();
        let keep = band_containing(2_000.0, 16, rate);
        isolator.set_selection(Some(BandIsolation {
            bands: 16,
            selected: vec![keep],
        }));
        let frames = rate as usize;
        let left = tone(2_000.0, frames, rate);
        let right = tone(80.0, frames, rate);
        let mut interleaved: Vec<f32> = left
            .iter()
            .zip(&right)
            .flat_map(|(l, r)| [*l, *r])
            .collect();
        isolator.process(&mut interleaved, rate, 2);
        let settled: Vec<f32> = interleaved[frames..].to_vec();
        let left_out: Vec<f32> = settled.iter().step_by(2).copied().collect();
        let right_out: Vec<f32> = settled.iter().skip(1).step_by(2).copied().collect();
        assert!(rms(&left_out) > 0.5);
        assert!(rms(&right_out) < 0.05);
    }
}
