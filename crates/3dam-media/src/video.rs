//! Video cheap-tier metadata + poster-frame thumbnail (PRODUCT_SPEC §9 phase 2b).
//!
//! Decode is a **discovered `ffprobe`/`ffmpeg` binary**, never a linked libav*
//! ([ADR 0015](../../../docs/adr/0015-video-decode-backend.md)). Everything here therefore has a
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

use dam_api::dto::{AudioAttributes, MediaAttributes, MediaType, VideoAttributes};
use serde_json::Value;
use std::path::Path;

use crate::proc::Tool;
use crate::HandlerError;

pub(crate) struct Handler;
pub(crate) static HANDLER: Handler = Handler;

impl crate::MediaHandler for Handler {
    fn media_type(&self) -> MediaType {
        MediaType::Video
    }

    fn detect(&self, path: &Path) -> Option<crate::FormatId> {
        let ext = crate::ext(path)?;
        let format = match ext.as_str() {
            "mkv" => "mkv",
            "webm" => "webm",
            "avi" => "avi",
            "ogv" => "ogv",
            "mp4" => "mp4",
            "mov" => "mov",
            "m4v" => "m4v",
            _ => return None,
        };
        Some(crate::FormatId {
            media: MediaType::Video,
            format,
            confidence: crate::Confidence::ExtensionOnly,
        })
    }

    fn extract_metadata(&self, path: &Path, format: &str) -> MediaAttributes {
        MediaAttributes::Video(metadata(path, format))
    }

    fn render_thumbnail(
        &self,
        path: &Path,
        _format: &str,
        max_edge: u32,
    ) -> Result<crate::ThumbPng, HandlerError> {
        let (bytes, width, height) = thumbnail(path, max_edge)?;
        Ok(crate::ThumbPng {
            bytes,
            width,
            height,
        })
    }
}

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
/// This is the `mp4`/`m4a` disambiguation ADR 0015 calls out: `.mp4` and `.mov` are containers, not
/// media types, and the audio decode matrix already claims `mp4`. `None` means "no prober, can't
/// tell" — the caller applies the documented extension fallback rather than guessing here.
///
/// A cover-art JPEG inside an audio file appears as a video stream, so an image-ish codec carrying
/// a single frame is not counted as video.
pub fn has_video_stream(path: &Path) -> Option<bool> {
    Some(VideoProbe::probe(path, &crate::MetadataBudget::default())?.has_video_stream())
}

fn is_real_video(stream: &Value) -> bool {
    if stream.get("codec_type").and_then(Value::as_str) != Some("video") {
        return false;
    }
    if stream
        .get("disposition")
        .and_then(|d| d.get("attached_pic"))
        .and_then(Value::as_i64)
        == Some(1)
    {
        return false;
    }
    let codec = stream
        .get("codec_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    !(matches!(codec, "mjpeg" | "png" | "bmp" | "gif" | "webp")
        && matches!(as_i64(stream.get("nb_frames")), Some(n) if n <= 1))
}

/// One parsed ffprobe response, reusable for classification and stream attributes.
#[derive(Clone, Debug)]
pub struct VideoProbe {
    document: Value,
}

impl VideoProbe {
    /// Probe once under a metadata budget, then reuse this value's classification
    /// and audio/video attributes. The combined ingest API also exposes I/O stats.
    pub fn probe(path: &Path, budget: &crate::MetadataBudget<'_>) -> Option<Self> {
        let mut session = crate::ingest::Session::new(budget);
        Self::ingest(path, &mut session).0
    }

    pub fn has_video_stream(&self) -> bool {
        streams(&self.document).iter().any(is_real_video)
    }

    pub fn video_attributes(&self) -> VideoAttributes {
        metadata_from_probe(&self.document)
    }

    pub fn audio_attributes(&self) -> AudioAttributes {
        let mut attrs = AudioAttributes::default();
        let format = self.document.get("format");
        attrs.duration_ms =
            as_f64(format.and_then(|f| f.get("duration"))).map(|s| (s * 1000.0).round() as i64);
        attrs.container = format
            .and_then(|f| f.get("format_name"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(stream) = stream_of(&self.document, "audio") {
            attrs.sample_rate = as_i64(stream.get("sample_rate"));
            attrs.channels = as_i64(stream.get("channels"));
            attrs.bit_depth = as_i64(stream.get("bits_per_raw_sample"))
                .filter(|n| *n > 0)
                .or_else(|| as_i64(stream.get("bits_per_sample")).filter(|n| *n > 0));
            attrs.codec = stream
                .get("codec_name")
                .and_then(Value::as_str)
                .map(str::to_string);
            if attrs.duration_ms.is_none() {
                attrs.duration_ms =
                    as_f64(stream.get("duration")).map(|s| (s * 1000.0).round() as i64);
            }
        }
        attrs
    }

    pub(crate) fn ingest(
        path: &Path,
        session: &mut crate::ingest::Session<'_, '_>,
    ) -> (Option<Self>, u32) {
        Self::ingest_with_tool(path, session, &FFPROBE)
    }

    fn ingest_with_tool(
        path: &Path,
        session: &mut crate::ingest::Session<'_, '_>,
        tool: &Tool,
    ) -> (Option<Self>, u32) {
        if !tool.available() || !session.active() || session.budget.max_bytes < 32 {
            return (None, 0);
        }
        let limit = session.remaining();
        // Reserve demuxer probe work in the same device pacing lane. The number is
        // a requested demuxer limit, never a measured physical read total.
        if limit < 32 || !(session.budget.before_read)(limit) || !session.active() {
            return (None, 0);
        }
        let args: [std::ffi::OsString; 11] = [
            "-v".into(), "error".into(), "-print_format".into(), "json".into(),
            "-probesize".into(), limit.to_string().into(),
            "-analyzeduration".into(), session.remaining_time().as_micros().to_string().into(),
            "-show_entries".into(),
            "format=duration,bit_rate,format_name:stream=codec_type,codec_name,width,height,avg_frame_rate,r_frame_rate,duration,nb_frames,sample_rate,channels,bits_per_sample,bits_per_raw_sample:stream_disposition=attached_pic".into(),
            path.as_os_str().to_os_string(),
        ];
        let output = tool.run_cancellable(
            args,
            session.remaining_time(),
            limit.min(256 * 1024) as u64,
            session.budget.cancelled,
        );
        (
            output
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .map(|document| Self { document }),
            1,
        )
    }
}

/// CHEAP tier: container/stream headers only, via one `ffprobe` invocation.
///
/// Returns a default (all-`None`) struct when ffprobe is absent. That is the designed floor, not a
/// failure: the asset is still catalogued, searchable by name, and playable.
pub fn metadata(path: &Path, _format: &str) -> VideoAttributes {
    probe(path)
        .map(|value| metadata_from_probe(&value))
        .unwrap_or_default()
}

fn metadata_from_probe(v: &Value) -> VideoAttributes {
    let mut attrs = VideoAttributes::default();

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

    if let Some(s) = streams(v).iter().find(|stream| is_real_video(stream)) {
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

    attrs.has_audio = Some(stream_of(v, "audio").is_some());
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
    #[test]
    fn classification_ignores_cover_art_but_keeps_real_mjpeg_and_later_video() {
        for (streams, expected) in [
            (serde_json::json!([{"codec_type":"audio"}]), false),
            (
                serde_json::json!([{"codec_type":"video","codec_name":"mjpeg","nb_frames":"1"}]),
                false,
            ),
            (
                serde_json::json!([{"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}}]),
                false,
            ),
            (
                serde_json::json!([{"codec_type":"video","codec_name":"mjpeg"}]),
                true,
            ),
            (
                serde_json::json!([{"codec_type":"video","codec_name":"mjpeg","nb_frames":"100"}]),
                true,
            ),
            (
                serde_json::json!([{"codec_type":"video","codec_name":"mjpeg","nb_frames":"1"},{"codec_type":"video","codec_name":"h264","width":1920}]),
                true,
            ),
        ] {
            let probe = VideoProbe {
                document: serde_json::json!({"streams":streams}),
            };
            assert_eq!(probe.has_video_stream(), expected);
        }
    }

    #[cfg(unix)]
    fn fake_tool(dir: &Path, body: &str) -> Tool {
        let path = dir.join("ffprobe");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        Tool::at_script(path)
    }

    #[cfg(unix)]
    #[test]
    fn one_probe_supplies_classification_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("calls");
        let tool = fake_tool(dir.path(), &format!(
            "echo call >> '{}'\nprintf '%s' '{{\"streams\":[{{\"codec_type\":\"video\",\"codec_name\":\"h264\",\"width\":1920,\"height\":1080,\"avg_frame_rate\":\"30/1\"}},{{\"codec_type\":\"audio\",\"sample_rate\":\"48000\",\"channels\":2}}],\"format\":{{\"duration\":\"2.5\"}}}}'", count.display()
        ));
        let budget = crate::MetadataBudget::default();
        let mut session = crate::ingest::Session::new(&budget);
        let (probe, invocations) =
            VideoProbe::ingest_with_tool(Path::new("container.mp4"), &mut session, &tool);
        let probe = probe.unwrap();
        assert!(probe.has_video_stream());
        assert_eq!(probe.video_attributes().width, Some(1920));
        assert_eq!(probe.video_attributes().duration_ms, Some(2500));
        assert_eq!(probe.audio_attributes().sample_rate, Some(48000));
        assert_eq!(invocations, 1);
        assert_eq!(std::fs::read_to_string(count).unwrap().lines().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn failed_probe_is_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("calls");
        let tool = fake_tool(
            dir.path(),
            &format!("echo call >> '{}'\nprintf '%s' 'not json'", count.display()),
        );
        let budget = crate::MetadataBudget::default();
        let mut session = crate::ingest::Session::new(&budget);
        let (probe, invocations) =
            VideoProbe::ingest_with_tool(Path::new("bad.mp4"), &mut session, &tool);
        assert!(probe.is_none());
        assert_eq!(invocations, 1);
        assert_eq!(std::fs::read_to_string(count).unwrap().lines().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_kills_a_running_probe_promptly() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let tool = fake_tool(dir.path(), "while :; do :; done");
        let cancelled = AtomicBool::new(false);
        let is_cancelled = || cancelled.load(Ordering::Relaxed);
        let budget = crate::MetadataBudget {
            timeout: std::time::Duration::from_secs(5),
            cancelled: &is_cancelled,
            ..crate::MetadataBudget::default()
        };
        let start = std::time::Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                cancelled.store(true, Ordering::Relaxed);
            });
            let mut session = crate::ingest::Session::new(&budget);
            let (probe, invocations) =
                VideoProbe::ingest_with_tool(Path::new("hanging.mp4"), &mut session, &tool);
            assert!(probe.is_none());
            assert_eq!(invocations, 1);
        });
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn closed_stdout_does_not_disarm_the_probe_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let tool = fake_tool(dir.path(), "exec 1>&-\nwhile :; do :; done");
        let budget = crate::MetadataBudget {
            timeout: std::time::Duration::from_millis(100),
            ..crate::MetadataBudget::default()
        };
        let start = std::time::Instant::now();
        let mut session = crate::ingest::Session::new(&budget);
        let (probe, _) =
            VideoProbe::ingest_with_tool(Path::new("hanging.mp4"), &mut session, &tool);
        assert!(probe.is_none());
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }
}
