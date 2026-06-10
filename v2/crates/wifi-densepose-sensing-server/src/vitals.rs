//! CSI phase-based breathing extraction.
//!
//! Chest motion modulates WiFi CSI phase in the 0.1-0.5 Hz band. This module
//! keeps a bounded rolling phase window and estimates the dominant respiratory
//! frequency with a Hann-windowed FFT.

use std::collections::VecDeque;
use std::f32::consts::PI;

use rustfft::{num_complex::Complex, FftPlanner};
use serde::Serialize;

const MAX_WINDOW_SAMPLES: usize = 512;
const FFT_WINDOW_SAMPLES: usize = 256;
const BREATHING_MIN_HZ: f32 = 0.1;
const BREATHING_MAX_HZ: f32 = 0.5;
const DEFAULT_MIN_CONFIDENCE: f32 = 0.3;

#[derive(Debug, Clone)]
pub struct BreathingExtractor {
    /// Rolling CSI phase values, capped to the last 512 samples.
    pub window: VecDeque<f32>,
    /// CSI phase sample rate in samples per second.
    pub sample_rate_hz: f32,
}

impl BreathingExtractor {
    pub fn new(sample_rate_hz: f32) -> Self {
        Self {
            window: VecDeque::with_capacity(MAX_WINDOW_SAMPLES),
            sample_rate_hz,
        }
    }

    pub fn push_sample(&mut self, phase: f32) {
        if !phase.is_finite() {
            return;
        }
        self.window.push_back(phase);
        while self.window.len() > MAX_WINDOW_SAMPLES {
            self.window.pop_front();
        }
    }

    pub fn extract_breathing(&self) -> BreathingResult {
        if self.window.len() < FFT_WINDOW_SAMPLES || self.sample_rate_hz <= f32::EPSILON {
            return BreathingResult::insufficient_data();
        }

        let mut samples: Vec<f32> = self
            .window
            .iter()
            .rev()
            .take(FFT_WINDOW_SAMPLES)
            .copied()
            .collect();
        samples.reverse();

        let mean = samples.iter().sum::<f32>() / samples.len() as f32;
        let mut buffer: Vec<Complex<f32>> = samples
            .iter()
            .enumerate()
            .map(|(idx, &sample)| {
                let hann = 0.5
                    * (1.0
                        - (2.0 * PI * idx as f32 / (FFT_WINDOW_SAMPLES as f32 - 1.0)).cos());
                Complex {
                    re: (sample - mean) * hann,
                    im: 0.0,
                }
            })
            .collect();

        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_WINDOW_SAMPLES);
        fft.process(&mut buffer);

        let freq_res = self.sample_rate_hz / FFT_WINDOW_SAMPLES as f32;
        let min_bin = (BREATHING_MIN_HZ / freq_res).ceil().max(1.0) as usize;
        let max_bin =
            ((BREATHING_MAX_HZ / freq_res).floor() as usize).min(FFT_WINDOW_SAMPLES / 2);
        if min_bin > max_bin || min_bin >= buffer.len() {
            return BreathingResult::insufficient_data();
        }

        let mut peak_bin = min_bin;
        let mut peak_amp = 0.0_f32;
        let mut band_sum = 0.0_f32;
        let mut band_count = 0_usize;

        for (bin, value) in buffer
            .iter()
            .enumerate()
            .take(max_bin + 1)
            .skip(min_bin)
        {
            let amp = value.norm();
            band_sum += amp;
            band_count += 1;
            if amp > peak_amp {
                peak_amp = amp;
                peak_bin = bin;
            }
        }

        if band_count == 0 || band_sum <= f32::EPSILON || peak_amp <= f32::EPSILON {
            return BreathingResult::insufficient_data();
        }

        let band_mean = band_sum / band_count as f32;
        let prominence = (peak_amp - band_mean).max(0.0);
        let confidence = if band_mean > f32::EPSILON {
            (prominence / band_mean).clamp(0.0, 1.0)
        } else {
            0.0
        };

        let peak_freq = interpolated_peak_frequency(&buffer, peak_bin, min_bin, max_bin, freq_res);
        let bpm = peak_freq * 60.0;
        BreathingResult {
            breathing_bpm: (confidence >= breathing_min_confidence()).then_some(bpm),
            confidence,
            method: BreathingResult::METHOD,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct BreathingResult {
    pub breathing_bpm: Option<f32>,
    pub confidence: f32,
    pub method: &'static str,
}

impl BreathingResult {
    pub const METHOD: &'static str = "fft_csi";

    pub fn insufficient_data() -> Self {
        Self {
            breathing_bpm: None,
            confidence: 0.0,
            method: Self::METHOD,
        }
    }
}

pub fn breathing_min_confidence() -> f32 {
    std::env::var("BREATHING_MIN_CONFIDENCE")
        .ok()
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(0.0, 1.0))
        .unwrap_or(DEFAULT_MIN_CONFIDENCE)
}

fn interpolated_peak_frequency(
    spectrum: &[Complex<f32>],
    peak_bin: usize,
    min_bin: usize,
    max_bin: usize,
    freq_res: f32,
) -> f32 {
    if peak_bin <= min_bin || peak_bin >= max_bin {
        return peak_bin as f32 * freq_res;
    }

    let alpha = spectrum[peak_bin - 1].norm();
    let beta = spectrum[peak_bin].norm();
    let gamma = spectrum[peak_bin + 1].norm();
    let denom = alpha - 2.0 * beta + gamma;
    if denom.abs() <= f32::EPSILON {
        return peak_bin as f32 * freq_res;
    }

    let offset = (0.5 * (alpha - gamma) / denom).clamp(-0.5, 0.5);
    (peak_bin as f32 + offset) * freq_res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deterministic_noise(seed: &mut u32) -> f32 {
        *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let unit = ((*seed >> 8) as f32) / ((u32::MAX >> 8) as f32);
        (unit - 0.5) * 0.2
    }

    #[test]
    fn detects_noisy_15_rpm_sine() {
        let sample_rate = 20.0;
        let mut extractor = BreathingExtractor::new(sample_rate);
        let mut seed = 42;

        for idx in 0..512 {
            let t = idx as f32 / sample_rate;
            let phase = (2.0 * PI * 0.25 * t).sin() + deterministic_noise(&mut seed);
            extractor.push_sample(phase);
        }

        let result = extractor.extract_breathing();
        let bpm = result
            .breathing_bpm
            .expect("0.25 Hz CSI phase sine should produce a breathing BPM");
        assert!(
            (bpm - 15.0).abs() <= 2.0,
            "expected 15 rpm +/- 2, got {bpm:.2}"
        );
        assert!(
            result.confidence >= breathing_min_confidence(),
            "expected confidence above threshold, got {:.3}",
            result.confidence
        );
        assert_eq!(result.method, "fft_csi");
    }

    #[test]
    fn flat_signal_has_low_confidence_and_no_bpm() {
        let mut extractor = BreathingExtractor::new(20.0);
        for _ in 0..512 {
            extractor.push_sample(0.42);
        }

        let result = extractor.extract_breathing();
        assert!(
            result.confidence < breathing_min_confidence(),
            "flat phase should stay below threshold, got {:.3}",
            result.confidence
        );
        assert!(result.breathing_bpm.is_none());
        assert_eq!(result.method, "fft_csi");
    }

    #[test]
    fn requires_256_samples() {
        let mut extractor = BreathingExtractor::new(20.0);
        for _ in 0..255 {
            extractor.push_sample(0.0);
        }

        let result = extractor.extract_breathing();
        assert!(result.breathing_bpm.is_none());
        assert_eq!(result.confidence, 0.0);
    }
}
