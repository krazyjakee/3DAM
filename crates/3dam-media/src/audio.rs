//! Audio cheap-tier metadata + decode-to-WAV convert (tech-spec 04 §5, §7.1; 08 §3.1).
//!
//! `metadata` probes the container and reads the default track's codec params (sample rate,
//! channels, bit depth, duration, codec/container names) — headers only, **no PCM decode** (the
//! CHEAP contract, §4). `convert` is the EXPENSIVE path: it decodes packets to PCM and writes a
//! canonical WAV (the lossless v1 encode target, 08 §3.1).

use dam_api::dto::{AudioAttributes, MediaAttributes, MediaType};
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::HandlerError;

pub(crate) struct Handler;
pub(crate) static HANDLER: Handler = Handler;

impl crate::MediaHandler for Handler {
    fn media_type(&self) -> MediaType {
        MediaType::Audio
    }

    fn detect(&self, path: &Path) -> Option<crate::FormatId> {
        let ext = crate::ext(path)?;
        let format = match ext.as_str() {
            "wav" | "flac" | "mp3" | "ogg" | "opus" | "aiff" | "m4a" | "aac" | "wma" | "it"
            | "xm" | "mod" | "s3m" => ext.as_str(),
            "oga" => "ogg",
            "aif" => "aiff",
            _ => return None,
        };
        Some(crate::FormatId {
            media: MediaType::Audio,
            format: canonical_format(format),
            confidence: crate::Confidence::ExtensionOnly,
        })
    }

    fn extract_metadata(&self, path: &Path, format: &str) -> MediaAttributes {
        MediaAttributes::Audio(metadata(path, format))
    }
}

fn canonical_format(format: &str) -> &'static str {
    match format {
        "wav" => "wav",
        "flac" => "flac",
        "mp3" => "mp3",
        "ogg" => "ogg",
        "opus" => "opus",
        "aiff" => "aiff",
        "m4a" => "m4a",
        "aac" => "aac",
        "wma" => "wma",
        "it" => "it",
        "xm" => "xm",
        "mod" => "mod",
        "s3m" => "s3m",
        _ => unreachable!("audio detection canonicalises every accepted extension"),
    }
}

/// Probe container + default-track params. Best-effort/fail-soft: an unreadable file yields the
/// default (all-`None`) struct rather than aborting the scan.
pub fn metadata(path: &Path, format: &str) -> AudioAttributes {
    let Ok(file) = std::fs::File::open(path) else {
        return AudioAttributes::default();
    };
    metadata_from_source(Box::new(file), format, 128 * 1024)
}

pub(crate) fn ingest_metadata(
    path: &Path,
    format: &str,
    session: &mut crate::ingest::Session<'_, '_>,
) -> AudioAttributes {
    if session.budget.max_allocation_bytes < 128 * 1024 {
        session.deferred = true;
        return AudioAttributes::default();
    }
    let Some((bytes, complete)) = session.prefix(path, session.remaining()) else {
        return AudioAttributes::default();
    };
    let mut attrs = metadata_from_source(
        Box::new(std::io::Cursor::new(bytes)),
        format,
        session.allocation_input_limit(),
    );
    if !complete {
        attrs.duration_ms = None;
    }
    attrs
}

fn metadata_from_source(
    source: Box<dyn symphonia::core::io::MediaSource>,
    format: &str,
    metadata_limit: usize,
) -> AudioAttributes {
    let mut attrs = AudioAttributes::default();
    let mss = MediaSourceStream::new(source, Default::default());
    let mut hint = Hint::new();
    hint.with_extension(format);
    let probed = symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions {
            limit_metadata_bytes: symphonia::core::meta::Limit::Maximum(metadata_limit),
            limit_visual_bytes: symphonia::core::meta::Limit::Maximum(metadata_limit),
        },
    );
    let Ok(probed) = probed else {
        return attrs;
    };
    let format_reader = probed.format;
    let Some(track) = format_reader
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
    else {
        return attrs;
    };
    let p = &track.codec_params;
    attrs.sample_rate = p.sample_rate.map(|v| v as i64);
    attrs.channels = p.channels.map(|c| c.count() as i64);
    attrs.bit_depth = p.bits_per_sample.map(|v| v as i64);
    if let (Some(n_frames), Some(sr)) = (p.n_frames, p.sample_rate) {
        if sr > 0 {
            attrs.duration_ms = Some((n_frames as i128 * 1000 / sr as i128) as i64);
        }
    }
    attrs.codec = symphonia::default::get_codecs()
        .get_codec(p.codec)
        .map(|d| d.short_name.to_string());
    attrs.container = Some(container_name(format).to_string());
    attrs
}

/// Map an extension to the container family name reported to the UI.
fn container_name(format: &str) -> &str {
    match format {
        "wav" => "wav",
        "flac" => "flac",
        "ogg" | "oga" => "ogg",
        "mp3" => "mp3",
        "opus" => "ogg",
        "m4a" | "aac" | "mp4" => "mp4",
        "aiff" | "aif" => "aiff",
        other => other,
    }
}

/// Decode to interleaved PCM and write a 16-bit WAV to `out` (convert pipeline, 08 §3.1). Returns
/// the number of frames written. This is the only audio path that fully decodes packets.
pub fn convert_to_wav<W: std::io::Write + std::io::Seek>(
    path: &Path,
    format: &str,
    out: W,
) -> Result<u64, HandlerError> {
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
    let mut format_reader = probed.format;
    let track = format_reader
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| HandlerError::Unsupported("no decodable audio track".into()))?;
    let track_id = track.id;
    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| HandlerError::Unsupported("unknown sample rate".into()))?;
    let channels = track
        .codec_params
        .channels
        .map(|c| c.count() as u16)
        .unwrap_or(2);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| HandlerError::Unsupported(e.to_string()))?;

    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer =
        hound::WavWriter::new(out, spec).map_err(|e| HandlerError::Encode(e.to_string()))?;

    let mut sample_buf: Option<SampleBuffer<i16>> = None;
    let mut frames: u64 = 0;
    loop {
        let packet = match format_reader.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break; // clean end of stream
            }
            Err(e) => return Err(HandlerError::Corrupt(e.to_string())),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                if sample_buf.is_none() {
                    let spec = *decoded.spec();
                    let duration = decoded.capacity() as u64;
                    sample_buf = Some(SampleBuffer::<i16>::new(duration, spec));
                }
                if let Some(buf) = sample_buf.as_mut() {
                    buf.copy_interleaved_ref(decoded);
                    for &s in buf.samples() {
                        writer
                            .write_sample(s)
                            .map_err(|e| HandlerError::Encode(e.to_string()))?;
                    }
                    frames += buf.len() as u64 / channels.max(1) as u64;
                }
            }
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue, // skip a bad packet
            Err(e) => return Err(HandlerError::Corrupt(e.to_string())),
        }
    }
    writer
        .finalize()
        .map_err(|e| HandlerError::Encode(e.to_string()))?;
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    // 200 ms, mono, 8 kHz AAC-LC in an ISO-BMFF/M4A container. Generated once with ffmpeg; tests
    // decode these committed bytes and have no runtime dependency on an external codec binary.
    const AAC_MP4: &str = "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";

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.m4a");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(AAC_MP4)
            .unwrap();
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn aac_in_mp4_has_metadata_pcm_convert_and_waveform_preview() {
        let (_dir, path) = fixture();
        let attrs = metadata(&path, "m4a");
        assert_eq!(attrs.container.as_deref(), Some("mp4"));
        assert_eq!(attrs.codec.as_deref(), Some("aac"));
        assert_eq!(attrs.sample_rate, Some(8_000));
        assert!(attrs.duration_ms.is_some_and(|duration| duration > 0));

        let wav = crate::convert_audio(&path, "m4a", "wav").unwrap();
        assert_eq!(&wav[..4], b"RIFF");
        let reader = hound::WavReader::new(std::io::Cursor::new(wav)).unwrap();
        assert_eq!(reader.spec().sample_rate, 8_000);
        assert!(reader.into_samples::<i16>().next().unwrap().is_ok());

        let peaks = crate::compute_waveform_peaks(&path, "m4a").unwrap();
        assert_eq!(peaks.len(), crate::audio_features::WAVEFORM_BUCKETS);
        assert!(peaks.iter().all(|value| value.is_finite()));
        assert!(peaks.iter().any(|value| *value > 0.0));
    }

    #[test]
    fn corrupt_aac_mp4_fails_softly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.m4a");
        std::fs::write(&path, b"not an mp4").unwrap();

        let attrs = metadata(&path, "m4a");
        assert!(attrs.duration_ms.is_none());
        assert!(attrs.sample_rate.is_none());
        assert!(attrs.channels.is_none());
        assert!(attrs.codec.is_none());
        assert!(attrs.container.is_none());
        assert!(crate::convert_audio(&path, "m4a", "wav").is_err());
        assert!(crate::compute_waveform_peaks(&path, "m4a").is_err());
    }
}
