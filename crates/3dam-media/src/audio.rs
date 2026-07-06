//! Audio cheap-tier metadata + decode-to-WAV convert (tech-spec 04 §5, §7.1; 08 §3.1).
//!
//! `metadata` probes the container and reads the default track's codec params (sample rate,
//! channels, bit depth, duration, codec/container names) — headers only, **no PCM decode** (the
//! CHEAP contract, §4). `convert` is the EXPENSIVE path: it decodes packets to PCM and writes a
//! canonical WAV (the lossless v1 encode target, 08 §3.1).

use dam_api::dto::AudioAttributes;
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::HandlerError;

/// Probe container + default-track params. Best-effort/fail-soft: an unreadable file yields the
/// default (all-`None`) struct rather than aborting the scan.
pub fn metadata(path: &Path, format: &str) -> AudioAttributes {
    let mut attrs = AudioAttributes::default();
    let Ok(file) = std::fs::File::open(path) else {
        return attrs;
    };
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(format);
    let probed = symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
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
