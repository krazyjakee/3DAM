//! Video cheap-tier metadata + poster-frame thumbnail (PRODUCT_SPEC §9 phase 2b).
//!
//! Decode is a **discovered `ffprobe`/`ffmpeg` binary**, never a linked libav*
//! ([ADR 0014](../../../docs/adr/0014-video-decode-backend.md)). Everything here therefore has a
//! defined answer when the tool is absent:
//!
//! | installed | metadata | thumbnail |
//! |---|---|---|
//! | nothing   | filesystem only (empty attrs) | typed tile |
//! | `ffprobe` | full cheap tier               | typed tile |
//! | both      | full cheap tier               | poster frame at ~10% duration |
//!
//! Playback never comes through here: the browser plays the original bytes via `<video>` off the
//! asset-content route, so we decode exactly one frame, exactly once, for the grid tile.

use dam_api::dto::VideoAttributes;
use serde_json::Value;
use std::path::Path;

use crate::proc::Tool;
use crate::HandlerError;

static FFPROBE: Tool = Tool::new("ffprobe", "DAM_FFPROBE");
static FFMPEG: Tool = Tool::new("ffmpeg", "DAM_FFMPEG");

/// Fraction of the running time to grab the poster frame at. Frame 0 of a real cutscene is very
/// often black (a fade-in) or a studio card, which makes for a uselessly uniform grid; 10% in is
/// past the intro on nearly everything while still being cheap to seek to.
const POSTER_AT: f64 = 0.10;

/// Whether a video decode backend is present at all. The UI uses this to explain an empty tile
/// honestly rather than letting it read as a bug.
pub fn probe_available() -> bool {
    FFPROBE.available()
}

/// Run `ffprobe` and parse its JSON. `None` when ffprobe is missing or the file defeats it.
fn probe(path: &Path) -> Option<Value> {
    let out = FFPROBE.run([
        "-v".as_ref(),
        "error".as_ref(),
        "-print_format".as_ref(),
        "json".as_ref(),
        "-show_format".as_ref(),
        "-show_streams".as_ref(),
        path.as_os_str(),
    ])?;
    serde_json::from_slice(&out).ok()
}

fn streams(v: &Value) -> &[Value] {
    v.get("streams")
        .and_then(|s| s.as_array())
        .map_or(&[], |a| a)
}

fn stream_of<'a>(v: &'a Value, kind: &str) -> Option<&'a Value> {
    streams(v)
        .iter()
        .find(|s| s.get("codec_type").and_then(|t| t.as_str()) == Some(kind))
}

/// `ffprobe` reports numbers as JSON strings in some fields and numbers in others depending on the
/// demuxer; accept either rather than silently dropping the value.
fn as_i64(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn as_f64(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Parse ffprobe's `"30000/1001"` rational frame rate. Returns `None` for the `0/0` ffprobe emits
/// when it genuinely doesn't know, so we report "unknown" instead of a confident 0 fps.
fn parse_rational(s: &str) -> Option<f32> {
    let (num, den) = s.split_once('/')?;
    let (num, den): (f64, f64) = (num.parse().ok()?, den.parse().ok()?);
    (den != 0.0 && num != 0.0).then_some((num / den) as f32)
}

/// Does this container carry a real video stream?
///
/// This is the `mp4`/`m4a` disambiguation ADR 0014 calls out: `.mp4` and `.mov` are containers, not
/// media types, and the audio decode matrix already claims `mp4`. `None` means "no prober, can't
/// tell" — the caller applies the documented extension fallback rather than guessing here.
///
/// A cover-art JPEG inside an audio file appears as a video stream, so an image-ish codec with no
/// real frame rate is not counted as video.
pub fn has_video_stream(path: &Path) -> Option<bool> {
    let v = probe(path)?;
    let Some(s) = stream_of(&v, "video") else {
        return Some(false);
    };
    let codec = s.get("codec_name").and_then(|c| c.as_str()).unwrap_or("");
    if matches!(codec, "mjpeg" | "png" | "bmp" | "gif" | "webp") {
        // Attached-picture disposition is ffprobe's explicit "this is cover art" marker.
        let cover = s
            .get("disposition")
            .and_then(|d| d.get("attached_pic"))
            .and_then(|a| a.as_i64())
            .unwrap_or(0);
        let frames = as_i64(s.get("nb_frames")).unwrap_or(0);
        if cover == 1 || frames <= 1 {
            return Some(false);
        }
    }
    Some(true)
}

/// CHEAP tier: container/stream headers only, via one `ffprobe` invocation.
///
/// Returns a default (all-`None`) struct when ffprobe is absent. That is the designed floor, not a
/// failure: the asset is still catalogued, searchable by name, and playable.
pub fn metadata(path: &Path, _format: &str) -> VideoAttributes {
    let mut attrs = VideoAttributes::default();
    let Some(v) = probe(path) else {
        return attrs;
    };

    if let Some(fmt) = v.get("format") {
        // Duration lives on the container; fall back to the video stream for formats (some MKV,
        // raw streams) that only carry it per-stream.
        attrs.duration_ms = as_f64(fmt.get("duration")).map(|s| (s * 1000.0).round() as i64);
        attrs.bitrate = as_i64(fmt.get("bit_rate"));
        attrs.container = fmt
            .get("format_name")
            .and_then(|f| f.as_str())
            .map(str::to_string);
    }

    if let Some(s) = stream_of(&v, "video") {
        attrs.width = as_i64(s.get("width"));
        attrs.height = as_i64(s.get("height"));
        attrs.codec = s
            .get("codec_name")
            .and_then(|c| c.as_str())
            .map(str::to_string);
        // `avg_frame_rate` is the honest average; `r_frame_rate` is a guessed base rate that reads
        // as e.g. 1000 fps on variable-frame-rate captures.
        attrs.fps = s
            .get("avg_frame_rate")
            .and_then(|f| f.as_str())
            .and_then(parse_rational)
            .or_else(|| {
                s.get("r_frame_rate")
                    .and_then(|f| f.as_str())
                    .and_then(parse_rational)
            });
        if attrs.duration_ms.is_none() {
            attrs.duration_ms = as_f64(s.get("duration")).map(|s| (s * 1000.0).round() as i64);
        }
    }

    attrs.has_audio = Some(stream_of(&v, "audio").is_some());
    attrs
}

/// EXPENSIVE tier: grab one frame at ~10% of the running time and return it as a PNG fitted into
/// `max_edge`.
///
/// Scaling happens inside ffmpeg rather than after the fact — a 4K frame decoded to PNG and handed
/// back over a pipe just to be shrunk here would cost tens of megabytes per thumbnail.
pub fn thumbnail(path: &Path, max_edge: u32) -> Result<(Vec<u8>, u32, u32), HandlerError> {
    if !FFMPEG.available() {
        return Err(HandlerError::Unsupported(
            "no ffmpeg found on PATH — video poster frames are unavailable in this environment \
             (set DAM_FFMPEG to point at it); the UI falls back to a typed tile"
                .to_string(),
        ));
    }

    // Seek to 10% when we know the duration. `-ss` before `-i` is the fast (keyframe) seek, which
    // is exactly right for a poster frame: we want *a* representative frame, not an exact one.
    let seek = metadata(path, "")
        .duration_ms
        .filter(|ms| *ms > 0)
        .map(|ms| format!("{:.3}", (ms as f64 / 1000.0) * POSTER_AT));

    let mut args: Vec<std::ffi::OsString> = Vec::new();
    args.push("-v".into());
    args.push("error".into());
    args.push("-nostdin".into());
    if let Some(ss) = &seek {
        args.push("-ss".into());
        args.push(ss.into());
    }
    args.push("-i".into());
    args.push(path.as_os_str().to_os_string());
    args.push("-frames:v".into());
    args.push("1".into());
    // Fit inside the box, preserve aspect, never upscale a small video to the tile size.
    args.push("-vf".into());
    args.push(
        format!(
            "scale=w={max_edge}:h={max_edge}:force_original_aspect_ratio=decrease:flags=bicubic"
        )
        .into(),
    );
    args.push("-f".into());
    args.push("image2".into());
    args.push("-c:v".into());
    args.push("png".into());
    args.push("-".into()); // PNG to stdout

    let bytes = FFMPEG.run(&args).ok_or_else(|| {
        HandlerError::Corrupt(format!(
            "ffmpeg could not extract a poster frame from {}",
            path.display()
        ))
    })?;
    if bytes.is_empty() {
        return Err(HandlerError::Corrupt(
            "ffmpeg produced an empty poster frame".to_string(),
        ));
    }

    // Read the dimensions back off the PNG we just made rather than recomputing what the scale
    // filter decided — the filter rounds, and the cache keys on the real size.
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(HandlerError::Io)?
        .into_dimensions()
        .map_err(|e| HandlerError::Corrupt(e.to_string()))?;
    Ok((bytes, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_frame_rates() {
        assert_eq!(parse_rational("30/1"), Some(30.0));
        assert!((parse_rational("30000/1001").unwrap() - 29.97).abs() < 0.01);
        // ffprobe's "I don't know" forms must not become a confident 0 fps.
        assert_eq!(parse_rational("0/0"), None);
        assert_eq!(parse_rational("0/1"), None);
        assert_eq!(parse_rational("garbage"), None);
    }

    #[test]
    fn numeric_fields_accept_string_or_number() {
        let v: Value = serde_json::json!({"a": "1920", "b": 1080, "c": true});
        assert_eq!(as_i64(v.get("a")), Some(1920));
        assert_eq!(as_i64(v.get("b")), Some(1080));
        assert_eq!(as_i64(v.get("c")), None);
        assert_eq!(as_f64(v.get("a")), Some(1920.0));
    }

    #[test]
    fn metadata_is_empty_but_valid_without_a_prober() {
        // Whatever the environment, a non-video file must yield a default struct and never panic.
        let attrs = metadata(Path::new("/nonexistent/nope.mp4"), "mp4");
        assert!(attrs.width.is_none() && attrs.duration_ms.is_none());
    }
}
