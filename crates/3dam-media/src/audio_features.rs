//! Audio feature extraction — the EXPENSIVE tier's analysis output for sound (tech-spec 04 §4
//! `extract_features`, consumed by the automation brain in tech-spec 05 §4.2). One full PCM decode
//! produces every *structural* audio signal the classifier needs, none of which is derivable from the
//! cheap-tier header metadata: whether the clip is a genuine **loop**, whether it is **tonal** and in
//! what key, whether it has a stable **tempo**, and whether its envelope is a one-shot transient or
//! sustained.
//!
//! **Two orthogonal axes, deliberately.** "What is it" (music / sfx / one-shot) and "does it loop"
//! are independent properties — a length threshold conflates them, which is why the v1 duration
//! heuristic mislabelled long non-looping music and long non-looping sound-effects alike as `loop`.
//! Here loopability is *measured* (authored `smpl` loop points when present, else a self-calibrating
//! seamlessness test at the wrap boundary) and content is judged from tonality + tempo + envelope.
//!
//! **Model-free.** This is pure DSP over `symphonia`-decoded PCM + `rustfft` — no weights, no network,
//! no system deps (ADR 0006). The semantic layer (CLAP zero-shot music/speech/sfx/genre labels via
//! `ort`) is the separate feature-gated model bump behind the same classify → suggest lifecycle; see
//! `crates/3dam-core/src/semantic.rs`. Everything below runs in the default offline build.

use rustfft::{num_complex::Complex, FftPlanner};
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::HandlerError;

/// Every derived audio signal from a single decode (tech-spec 05 §4.2).
#[derive(Clone, Debug)]
pub struct AudioFeatures {
    /// Coarse content+structure class: `one_shot` | `loop` | `music` | `sfx`.
    pub class: &'static str,
    /// True when the clip is a genuine loop — authored loop points *or* a seamless wrap boundary.
    pub is_loop: bool,
    /// How the loop was established, for provenance/confidence.
    pub loop_source: LoopSource,
    /// Seamlessness score in [0,1] (1 = a hard tail→head splice is inaudible). Meaningless when
    /// `loop_source == Metadata` (authored points are authoritative and skip the measurement).
    pub loopability: f32,
    /// Number of detected onsets (attacks). One (or zero) is the one-shot signature.
    pub onset_count: u32,
    /// Estimated tempo in BPM when a stable pulse is present, else `None` (arrhythmic / non-musical).
    pub bpm: Option<f32>,
    /// Dominant chroma has a clear tonal centre (pitched content) vs. flat chroma (noise/percussion).
    pub tonal: bool,
    /// Dominant pitch class (`c`, `c#`, … `b`) when `tonal`, else `None`.
    pub key: Option<&'static str>,
    /// Envelope is sustained (music/pad/drone) rather than a front-loaded decaying transient.
    pub sustained: bool,
    /// Clip duration in seconds (from decoded PCM, not the header).
    pub duration_s: f32,
}

/// How loopability was decided (tech-spec 05 §4.2). Authored metadata outranks measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopSource {
    /// A WAV `smpl` chunk declared ≥1 sample loop — the author's own intent, authoritative.
    Metadata,
    /// No metadata, but the wrap boundary is seamless by measurement.
    Seamless,
    /// Not a loop.
    None,
}

// --- Tuning constants (v1 defaults; §8, tuned later). ------------------------------------------

/// STFT frame / hop for the spectral passes — ~43 ms / ~11 ms at 48 kHz.
const FRAME: usize = 2048;
const HOP: usize = 512;
/// Cap the decode to bound memory on pathological inputs (a 10-min mono f32 buffer ≈ 115 MB). Longer
/// files are analysed on their head; loops in game audio are short so this is a safe degrade.
const MAX_SECONDS: usize = 600;
/// Seamlessness cutoff above which a clip with no authored loop points is still called a loop.
const LOOP_THRESHOLD: f32 = 0.62;
/// A loop must not decay to near-silence at its tail (that is a one-shot with a ring-out, not a loop).
const FADE_FLOOR: f32 = 0.35;
/// Chroma peakiness (dominant pitch-class energy ÷ mean) above which a clip reads as tonal.
const TONAL_RATIO: f32 = 2.6;

const PITCH_CLASSES: [&str; 12] = [
    "c", "c#", "d", "d#", "e", "f", "f#", "g", "g#", "a", "a#", "b",
];

/// Decode `path` once and derive every audio signal. EXPENSIVE — only called by the analysis pass,
/// never at ingest scale. Fail-soft is the caller's job: a decode error returns `Corrupt` and the
/// asset keeps its cheap-tier metadata with no derived class (tech-spec 05 §2.3).
pub fn extract_audio_features(path: &Path, format: &str) -> Result<AudioFeatures, HandlerError> {
    let (samples, sr) = decode_mono(path, format)?;
    if samples.is_empty() || sr == 0 {
        return Err(HandlerError::Corrupt("no decodable audio frames".into()));
    }
    let duration_s = samples.len() as f32 / sr as f32;

    // Authored loop points win outright — the author already answered the question (§4.2).
    let meta_loop =
        (format.eq_ignore_ascii_case("wav")) && wav_has_sample_loop(path).unwrap_or(false);

    // One STFT pass yields the onset envelope (flux per frame), the chroma vector, and the first/last
    // frame magnitudes needed for the wrap-boundary seamlessness test.
    let spec = stft_pass(&samples, sr);

    let (tonal, key) = tonality(&spec.chroma);
    let onset_count = count_onsets(&spec.flux);
    let frame_rate = sr as f32 / HOP as f32;
    let bpm = estimate_bpm(&spec.flux, frame_rate);
    let sustained = is_sustained(&samples, sr);

    let (loopability, loop_source) = if meta_loop {
        (1.0, LoopSource::Metadata)
    } else {
        let score = seamlessness(&samples, &spec);
        if score >= LOOP_THRESHOLD {
            (score, LoopSource::Seamless)
        } else {
            (score, LoopSource::None)
        }
    };
    let is_loop = loop_source != LoopSource::None;

    let class = classify(is_loop, onset_count, bpm, tonal, sustained, duration_s);

    Ok(AudioFeatures {
        class,
        is_loop,
        loop_source,
        loopability,
        onset_count,
        bpm,
        tonal,
        key,
        sustained,
        duration_s,
    })
}

/// The coarse class from the orthogonal signals (tech-spec 05 §4.2). Order matters: "it loops" is the
/// salient game-audio fact, so a musical loop reads as `loop` (the `tonal`/`rhythmic` tags still make
/// it discoverable as music). A lone transient is a `one_shot`; sustained tonal-or-metred content that
/// does *not* loop is `music`; the rest is `sfx`.
fn classify(
    is_loop: bool,
    onset_count: u32,
    bpm: Option<f32>,
    tonal: bool,
    sustained: bool,
    duration_s: f32,
) -> &'static str {
    if is_loop {
        "loop"
    } else if onset_count <= 1 && !sustained && duration_s < 1.5 {
        "one_shot"
    } else if tonal && (bpm.is_some() || (sustained && duration_s >= 4.0)) {
        "music"
    } else if !sustained && duration_s < 1.5 {
        "one_shot"
    } else {
        "sfx"
    }
}

// --- Decode ------------------------------------------------------------------------------------

/// Decode any supported container to a single mono f32 channel (downmix by averaging), returning the
/// samples and sample rate. The only audio path besides `convert` that fully decodes packets. Public
/// so the semantic tier's mel front-end ([`crate::log_mel`]) can reuse the one decode path.
pub fn decode_mono(path: &Path, format: &str) -> Result<(Vec<f32>, u32), HandlerError> {
    let file = std::fs::File::open(path).map_err(HandlerError::Io)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(format);
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| HandlerError::Corrupt(e.to_string()))?;
    let mut reader = probed.format;
    let track = reader
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| HandlerError::Unsupported("no decodable audio track".into()))?;
    let track_id = track.id;
    let sr = track
        .codec_params
        .sample_rate
        .ok_or_else(|| HandlerError::Unsupported("unknown sample rate".into()))?;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| HandlerError::Unsupported(e.to_string()))?;

    let cap = sr as usize * MAX_SECONDS;
    let mut mono: Vec<f32> = Vec::new();
    let mut sbuf: Option<SampleBuffer<f32>> = None;
    loop {
        let packet = match reader.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(e) => return Err(HandlerError::Corrupt(e.to_string())),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                let ch = decoded.spec().channels.count().max(1);
                if sbuf.is_none() {
                    sbuf = Some(SampleBuffer::<f32>::new(
                        decoded.capacity() as u64,
                        *decoded.spec(),
                    ));
                }
                if let Some(buf) = sbuf.as_mut() {
                    buf.copy_interleaved_ref(decoded);
                    for frame in buf.samples().chunks(ch) {
                        mono.push(frame.iter().sum::<f32>() / ch as f32);
                    }
                }
                if mono.len() >= cap {
                    break; // head-only analysis on pathologically long inputs
                }
            }
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue, // skip a bad packet
            Err(e) => return Err(HandlerError::Corrupt(e.to_string())),
        }
    }
    Ok((mono, sr))
}

// --- STFT-derived signals ----------------------------------------------------------------------

struct StftPass {
    /// Spectral flux per frame boundary — the onset detection function (rectified spectral change).
    flux: Vec<f32>,
    /// Magnitude-weighted 12-bin chroma, accumulated over all frames.
    chroma: [f32; 12],
    /// Magnitude spectrum of the first and last frames, for the wrap-boundary flux.
    first_mag: Vec<f32>,
    last_mag: Vec<f32>,
}

/// One windowed FFT sweep. Accumulates the onset envelope and chroma while keeping only the rolling
/// previous frame plus the first/last frames — O(frames) memory, not O(frames × bins).
fn stft_pass(samples: &[f32], sr: u32) -> StftPass {
    let bins = FRAME / 2 + 1;
    let window = hann(FRAME);
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FRAME);
    let mut scratch = vec![Complex::new(0.0f32, 0.0); FRAME];

    // Precompute each FFT bin's pitch class (or None for out-of-range bins) once.
    let bin_pc: Vec<Option<usize>> = (0..bins).map(|k| bin_pitch_class(k, sr)).collect();

    let mut flux: Vec<f32> = Vec::new();
    let mut chroma = [0.0f32; 12];
    let mut prev: Option<Vec<f32>> = None;
    let mut first_mag: Vec<f32> = Vec::new();
    let mut last_mag: Vec<f32> = Vec::new();

    let mut pos = 0;
    while pos + FRAME <= samples.len() {
        for (i, s) in scratch.iter_mut().enumerate() {
            *s = Complex::new(samples[pos + i] * window[i], 0.0);
        }
        fft.process(&mut scratch);
        let mag: Vec<f32> = scratch[..bins].iter().map(|c| c.norm()).collect();

        for (k, &m) in mag.iter().enumerate() {
            if let Some(pc) = bin_pc[k] {
                chroma[pc] += m;
            }
        }
        if let Some(p) = &prev {
            let f: f32 = mag.iter().zip(p).map(|(m, pm)| (m - pm).max(0.0)).sum();
            flux.push(f);
        }
        if first_mag.is_empty() {
            first_mag = mag.clone();
        }
        last_mag = mag.clone();
        prev = Some(mag);
        pos += HOP;
    }

    StftPass {
        flux,
        chroma,
        first_mag,
        last_mag,
    }
}

/// Chroma peakiness → (is_tonal, dominant pitch class). A single strong pitch class (a sine, a chord
/// root) sits well above the mean; flat chroma (noise, unpitched percussion) does not.
fn tonality(chroma: &[f32; 12]) -> (bool, Option<&'static str>) {
    let sum: f32 = chroma.iter().sum();
    if sum <= f32::EPSILON {
        return (false, None);
    }
    let mean = sum / 12.0;
    let (idx, &peak) = chroma
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .unwrap();
    if peak >= mean * TONAL_RATIO {
        (true, Some(PITCH_CLASSES[idx]))
    } else {
        (false, None)
    }
}

/// Map an FFT bin to a pitch class, ignoring sub-bass/hiss outside the musically salient band.
fn bin_pitch_class(k: usize, sr: u32) -> Option<usize> {
    let freq = k as f32 * sr as f32 / FRAME as f32;
    if !(40.0..=5000.0).contains(&freq) {
        return None;
    }
    let midi = 69.0 + 12.0 * (freq / 440.0).log2();
    let pc = (midi.round() as i64).rem_euclid(12);
    Some(pc as usize)
}

/// Count onsets: peaks in the flux envelope above an adaptive threshold, spaced ≥ ~50 ms apart so a
/// single attack's ringing does not read as many onsets. One (or zero) onset is the one-shot tell.
fn count_onsets(flux: &[f32]) -> u32 {
    if flux.is_empty() {
        return 0;
    }
    let mean = flux.iter().sum::<f32>() / flux.len() as f32;
    let var = flux.iter().map(|f| (f - mean).powi(2)).sum::<f32>() / flux.len() as f32;
    let thresh = mean + 1.5 * var.sqrt();
    let min_gap = (0.05 * (FRAME as f32 / HOP as f32 * 4.0)).max(3.0) as usize; // ~50 ms in frames
    let mut count = 0u32;
    let mut last = None::<usize>;
    for i in 1..flux.len().saturating_sub(1) {
        if flux[i] > thresh
            && flux[i] >= flux[i - 1]
            && flux[i] > flux[i + 1]
            && last.is_none_or(|l| i - l >= min_gap)
        {
            count += 1;
            last = Some(i);
        }
    }
    count
}

/// Estimate tempo by autocorrelating the onset envelope and taking the strongest lag in the 50–200
/// BPM band. Returns `None` when no lag is prominent (arrhythmic / non-musical).
fn estimate_bpm(flux: &[f32], frame_rate: f32) -> Option<f32> {
    if flux.len() < 16 {
        return None;
    }
    let mean = flux.iter().sum::<f32>() / flux.len() as f32;
    let centred: Vec<f32> = flux.iter().map(|f| f - mean).collect();
    let denom: f32 = centred.iter().map(|x| x * x).sum();
    if denom < 1e-6 {
        return None;
    }
    // BPM range → lag range (frames). lag = frame_rate * 60 / bpm.
    let lag_min = (frame_rate * 60.0 / 200.0).round().max(1.0) as usize;
    let lag_max = ((frame_rate * 60.0 / 50.0).round() as usize).min(flux.len() / 2);
    let mut best = (0usize, 0.0f32);
    for lag in lag_min..lag_max {
        let mut acc = 0.0f32;
        for i in 0..(centred.len() - lag) {
            acc += centred[i] * centred[i + lag];
        }
        let corr = acc / denom;
        if corr > best.1 {
            best = (lag, corr);
        }
    }
    // Require a reasonably prominent pulse, else call it arrhythmic.
    if best.0 > 0 && best.1 >= 0.30 {
        Some(frame_rate * 60.0 / best.0 as f32)
    } else {
        None
    }
}

/// Envelope shape: sustained (roughly level / music) vs. a front-loaded decaying transient (one-shot).
/// Windowed RMS; a clip whose energy peaks early and decays to a fraction of that peak is transient.
fn is_sustained(samples: &[f32], sr: u32) -> bool {
    let win = (sr as usize / 20).max(1); // ~50 ms
    let rms: Vec<f32> = samples
        .chunks(win)
        .map(|c| (c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    if rms.len() < 3 {
        return false;
    }
    let (peak_idx, &peak) = rms
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .unwrap();
    if peak <= f32::EPSILON {
        return false;
    }
    let front_loaded = peak_idx < rms.len() / 4;
    let tail = rms.last().copied().unwrap_or(0.0);
    // Transient = attack near the front that decays away; anything else counts as sustained.
    !(front_loaded && tail < 0.3 * peak)
}

/// Seamlessness in [0,1]: how inaudible a hard tail→head splice would be (tech-spec 05 §4.2). Two
/// scale-free measurements — the splice amplitude step and the spectral similarity of the first/last
/// frames — hard-gated on a fade-out (a tail that rings out to silence cannot loop). Scale-free on
/// purpose: a self-calibrating "vs. interior" test collapses for perfectly steady tones (near-zero
/// interior change), which are in fact the *most* loopable signals.
fn seamlessness(samples: &[f32], spec: &StftPass) -> f32 {
    let n = samples.len();
    if n < FRAME * 2 {
        return 0.0;
    }
    // Fade guard: compare tail energy to overall. A ring-out to silence is a one-shot, not a loop.
    let overall = rms(samples);
    let tail = rms(&samples[n.saturating_sub(FRAME)..]);
    let head = rms(&samples[..FRAME]);
    if overall <= f32::EPSILON || head <= f32::EPSILON || tail < FADE_FLOOR * overall {
        return 0.0;
    }

    // Splice amplitude continuity: the actual joint x[n-1]→x[0]. A large step is an audible click.
    let peak = samples
        .iter()
        .fold(0.0f32, |m, &s| m.max(s.abs()))
        .max(1e-6);
    let disc = (samples[0] - samples[n - 1]).abs() / peak;
    let amp_component = (1.0 - disc).clamp(0.0, 1.0);

    // Spectral continuity of the wrap: how alike the first and last frame magnitude spectra are
    // (normalised, so it is a similarity in [0,1] regardless of overall level).
    let num: f32 = spec
        .first_mag
        .iter()
        .zip(&spec.last_mag)
        .map(|(a, b)| (a - b).abs())
        .sum();
    let den: f32 = spec.first_mag.iter().chain(&spec.last_mag).sum::<f32>() + 1e-6;
    let spec_component = (1.0 - num / den).clamp(0.0, 1.0);

    0.5 * spec_component + 0.5 * amp_component
}

fn rms(s: &[f32]) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
}

fn hann(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let x = std::f32::consts::PI * i as f32 / (n as f32 - 1.0);
            x.sin().powi(2)
        })
        .collect()
}

// --- WAV `smpl` chunk (authored loop points) ---------------------------------------------------

/// Return `Ok(true)` when a RIFF/WAVE file carries a `smpl` chunk declaring ≥1 sample loop — the
/// author's own loop intent, which outranks any measurement (tech-spec 05 §4.2). Best-effort: any
/// parse hiccup yields `None` and the seamlessness measurement takes over. Reads only the small chunk
/// headers, never the PCM body.
fn wav_has_sample_loop(path: &Path) -> Option<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let mut riff = [0u8; 12];
    f.read_exact(&mut riff).ok()?;
    if &riff[0..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
        return Some(false);
    }
    loop {
        let mut hdr = [0u8; 8];
        if f.read_exact(&mut hdr).is_err() {
            return Some(false); // walked off the end without finding smpl
        }
        let id = &hdr[0..4];
        let size = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as u64;
        if id == b"smpl" {
            // The loop count is the 8th u32 of the smpl chunk body.
            let mut body = [0u8; 36];
            f.read_exact(&mut body).ok()?;
            let num_loops = u32::from_le_bytes([body[28], body[29], body[30], body[31]]);
            return Some(num_loops > 0);
        }
        // Skip to the next chunk (chunks are word-aligned: pad an odd size by one byte).
        let advance = size + (size & 1);
        f.seek(SeekFrom::Current(advance as i64)).ok()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    fn tmp(name: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("3dam-af-{}-{n}-{name}", std::process::id()))
    }

    /// Write a mono 16-bit WAV from f32 samples in [-1,1].
    fn write_wav(path: &Path, samples: &[f32], sr: u32) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: sr,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for &s in samples {
            w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
                .unwrap();
        }
        w.finalize().unwrap();
    }

    fn sine(freq: f32, secs: f32, sr: u32) -> Vec<f32> {
        let n = (secs * sr as f32) as usize;
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / sr as f32).sin() * 0.6)
            .collect()
    }

    #[test]
    fn seamless_tonal_sine_is_a_loop() {
        // An integer number of cycles wraps perfectly and has one strong pitch class.
        let sr = 44_100;
        // 441 Hz over exactly 1.0 s at 44.1 kHz = 441 whole cycles → seamless wrap.
        let s = sine(441.0, 1.0, sr);
        let p = tmp("sine.wav");
        write_wav(&p, &s, sr);
        let f = extract_audio_features(&p, "wav").unwrap();
        assert!(f.tonal, "a sine is tonal");
        assert_eq!(f.key, Some("a")); // 441 Hz ≈ A4
        assert!(
            f.is_loop,
            "integer-cycle sine wraps seamlessly: {}",
            f.loopability
        );
        assert_eq!(f.class, "loop");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn decaying_click_is_a_one_shot() {
        // A short front-loaded exponential decay: one onset, transient envelope, fades out.
        let sr = 44_100;
        let n = (0.4 * sr as f32) as usize;
        let s: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                (2.0 * std::f32::consts::PI * 800.0 * t).sin() * (-t * 18.0).exp()
            })
            .collect();
        let p = tmp("click.wav");
        write_wav(&p, &s, sr);
        let f = extract_audio_features(&p, "wav").unwrap();
        assert!(!f.sustained, "a decaying click is transient");
        assert!(!f.is_loop, "a fade-out cannot loop: {}", f.loopability);
        assert_eq!(f.class, "one_shot");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn noise_bursts_are_sfx_not_loop() {
        // Several separated broadband bursts (typing-like): many onsets, atonal, not seamless.
        let sr = 44_100;
        let mut s = vec![0.0f32; (2.5 * sr as f32) as usize];
        let mut state = 0x1234_5678u32;
        for burst in 0..6 {
            let start = burst * sr as usize / 3;
            for i in 0..(sr as usize / 40) {
                // xorshift noise, no std rng needed
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let noise = (state as f32 / u32::MAX as f32) * 2.0 - 1.0;
                if start + i < s.len() {
                    s[start + i] = noise * 0.7;
                }
            }
        }
        let p = tmp("typing.wav");
        write_wav(&p, &s, sr);
        let f = extract_audio_features(&p, "wav").unwrap();
        assert!(!f.tonal, "broadband bursts are atonal");
        assert!(
            f.onset_count >= 2,
            "multiple bursts → multiple onsets: {}",
            f.onset_count
        );
        assert_ne!(f.class, "one_shot");
        assert_ne!(f.class, "music");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn sustained_tonal_nonlooping_is_music() {
        // Long sustained tone with a hard fade only at the very end → tonal + sustained + not seamless.
        let sr = 44_100;
        let mut s = sine(330.0, 6.0, sr); // ~E4, 6 s
        let n = s.len();
        let fade = sr as usize / 2;
        for (k, v) in s[n - fade..].iter_mut().enumerate() {
            *v *= 1.0 - k as f32 / fade as f32; // ramp to silence → breaks the wrap
        }
        let p = tmp("music.wav");
        write_wav(&p, &s, sr);
        let f = extract_audio_features(&p, "wav").unwrap();
        assert!(f.tonal);
        assert!(
            !f.is_loop,
            "the tail fades, so it is not seamless: {}",
            f.loopability
        );
        assert_eq!(f.class, "music");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn smpl_chunk_forces_loop() {
        // hound doesn't write smpl, so hand-check the parser stays false on a plain WAV.
        let sr = 22_050;
        let s = sine(220.0, 0.5, sr);
        let p = tmp("plain.wav");
        write_wav(&p, &s, sr);
        assert_eq!(wav_has_sample_loop(&p), Some(false));
        std::fs::remove_file(&p).ok();
    }
}
