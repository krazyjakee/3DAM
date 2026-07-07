//! Log-mel spectrogram — the audio front-end every neural audio tower expects (CLAP, Whisper, PANNs).
//! Pure `symphonia` decode + `rustfft`, no model, no system deps: the CPU-side preprocessing that
//! turns a decoded waveform into the `[n_mels, n_frames]` feature map an ONNX/candle audio encoder
//! consumes (tech-spec 05 §2.3, the CLAP sub-path of semantic-search M4).
//!
//! It is model-agnostic — [`MelConfig`] carries the checkpoint's parameters (sample rate, FFT size,
//! hop, mel-band count and range), so CLAP, Whisper, and friends each pass their own. Decoupled from
//! the classifier's cheap DSP in [`crate::audio_features`] on purpose: that tier is model-free and
//! always on; this one only runs when a model is present to consume its output.

use crate::HandlerError;
use rustfft::{num_complex::Complex, FftPlanner};
use std::path::Path;

/// The preprocessing parameters of a specific audio encoder checkpoint.
#[derive(Clone, Copy, Debug)]
pub struct MelConfig {
    /// Target sample rate; the decoded audio is resampled to this (CLAP = 48 kHz).
    pub sample_rate: u32,
    /// FFT window length in samples.
    pub n_fft: usize,
    /// Hop between frames in samples.
    pub hop: usize,
    /// Number of mel bands.
    pub n_mels: usize,
    /// Mel filterbank low/high edge in Hz.
    pub fmin: f32,
    pub fmax: f32,
}

/// A log-mel spectrogram: `data` is row-major `[n_mels][n_frames]`, natural-log of mel energies.
#[derive(Clone, Debug)]
pub struct MelSpectrogram {
    pub data: Vec<f32>,
    pub n_mels: usize,
    pub n_frames: usize,
}

/// Decode `path`, resample to `cfg.sample_rate`, and compute its log-mel spectrogram. EXPENSIVE
/// (full decode) — a semantic-tier preprocessing step, never called at ingest scale.
pub fn log_mel(path: &Path, format: &str, cfg: &MelConfig) -> Result<MelSpectrogram, HandlerError> {
    let (samples, sr) = crate::audio_features::decode_mono(path, format)?;
    if samples.is_empty() {
        return Err(HandlerError::Corrupt("no decodable audio frames".into()));
    }
    let samples = resample_linear(&samples, sr, cfg.sample_rate);
    Ok(mel_from_samples(&samples, cfg))
}

/// Compute the log-mel spectrogram of already-decoded, already-resampled mono samples.
pub fn mel_from_samples(samples: &[f32], cfg: &MelConfig) -> MelSpectrogram {
    let window = hann(cfg.n_fft);
    let filters = mel_filterbank(cfg);
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(cfg.n_fft);
    let bins = cfg.n_fft / 2 + 1;

    // Frame count: at least one frame (zero-pad a short clip up to n_fft).
    let n_frames = if samples.len() < cfg.n_fft {
        1
    } else {
        (samples.len() - cfg.n_fft) / cfg.hop + 1
    };

    let mut data = vec![0.0f32; cfg.n_mels * n_frames];
    let mut scratch = vec![Complex::new(0.0f32, 0.0); cfg.n_fft];
    for frame in 0..n_frames {
        let start = frame * cfg.hop;
        for (i, s) in scratch.iter_mut().enumerate() {
            let v = samples.get(start + i).copied().unwrap_or(0.0);
            *s = Complex::new(v * window[i], 0.0);
        }
        fft.process(&mut scratch);
        // Power spectrum over the non-redundant half.
        let power: Vec<f32> = scratch[..bins].iter().map(|c| c.norm_sqr()).collect();
        for m in 0..cfg.n_mels {
            let e: f32 = filters[m].iter().map(|&(k, w)| power[k] * w).sum();
            data[m * n_frames + frame] = (e + 1e-10).ln();
        }
    }
    MelSpectrogram {
        data,
        n_mels: cfg.n_mels,
        n_frames,
    }
}

/// Triangular mel filterbank as sparse `(bin, weight)` lists per mel band (Slaney-style, on the
/// `2595·log10(1+f/700)` scale).
fn mel_filterbank(cfg: &MelConfig) -> Vec<Vec<(usize, f32)>> {
    let bins = cfg.n_fft / 2 + 1;
    let hz_to_mel = |f: f32| 2595.0 * (1.0 + f / 700.0).log10();
    let mel_to_hz = |m: f32| 700.0 * (10f32.powf(m / 2595.0) - 1.0);
    let m_min = hz_to_mel(cfg.fmin);
    let m_max = hz_to_mel(cfg.fmax);
    // n_mels+2 edge points → n_mels overlapping triangles.
    let edges: Vec<f32> = (0..cfg.n_mels + 2)
        .map(|i| {
            let m = m_min + (m_max - m_min) * i as f32 / (cfg.n_mels + 1) as f32;
            mel_to_hz(m)
        })
        .collect();
    let bin_hz = cfg.sample_rate as f32 / cfg.n_fft as f32;
    let mut fb = vec![Vec::new(); cfg.n_mels];
    for m in 0..cfg.n_mels {
        let (lo, ctr, hi) = (edges[m], edges[m + 1], edges[m + 2]);
        for k in 0..bins {
            let f = k as f32 * bin_hz;
            let w = if f >= lo && f <= ctr {
                (f - lo) / (ctr - lo).max(1e-6)
            } else if f > ctr && f <= hi {
                (hi - f) / (hi - ctr).max(1e-6)
            } else {
                0.0
            };
            if w > 0.0 {
                fb[m].push((k, w));
            }
        }
    }
    fb
}

/// Naive linear resample. Adequate for a mel front-end (the mel pooling is far coarser than any
/// resampling artefact); a polyphase resampler is a later refinement if a checkpoint needs it.
fn resample_linear(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || from == 0 {
        return samples.to_vec();
    }
    let ratio = to as f64 / from as f64;
    let out_len = (samples.len() as f64 * ratio) as usize;
    (0..out_len)
        .map(|i| {
            let src = i as f64 / ratio;
            let i0 = src.floor() as usize;
            let frac = (src - i0 as f64) as f32;
            let a = samples.get(i0).copied().unwrap_or(0.0);
            let b = samples.get(i0 + 1).copied().unwrap_or(a);
            a + (b - a) * frac
        })
        .collect()
}

fn hann(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let x = std::f32::consts::PI * i as f32 / (n as f32 - 1.0);
            x.sin().powi(2)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clap_cfg() -> MelConfig {
        MelConfig {
            sample_rate: 48_000,
            n_fft: 1024,
            hop: 480,
            n_mels: 64,
            fmin: 50.0,
            fmax: 14_000.0,
        }
    }

    #[test]
    fn mel_shape_and_finiteness() {
        let sr = 48_000;
        let n = sr; // 1 s
        let sig: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr as f32).sin() * 0.5)
            .collect();
        let m = mel_from_samples(&sig, &clap_cfg());
        assert_eq!(m.n_mels, 64);
        assert_eq!(m.data.len(), m.n_mels * m.n_frames);
        assert!(m.data.iter().all(|v| v.is_finite()), "log-mel is finite");
    }

    #[test]
    fn tone_energises_its_own_band_more_than_a_distant_one() {
        // A 1 kHz tone should light up a low-ish mel band far more than the top band.
        let sr = 48_000;
        let sig: Vec<f32> = (0..sr)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr as f32).sin() * 0.5)
            .collect();
        let cfg = clap_cfg();
        let m = mel_from_samples(&sig, &cfg);
        let row_mean = |band: usize| {
            (0..m.n_frames)
                .map(|f| m.data[band * m.n_frames + f])
                .sum::<f32>()
                / m.n_frames as f32
        };
        // 1 kHz sits low on the mel scale; the very top band (near 14 kHz) sees little energy.
        assert!(
            row_mean(8) > row_mean(63),
            "1 kHz band hotter than the top band"
        );
    }

    #[test]
    fn resample_changes_length_by_ratio() {
        let s = vec![0.0f32; 48_000];
        let out = resample_linear(&s, 48_000, 24_000);
        assert!((out.len() as i64 - 24_000).abs() <= 1);
    }
}
