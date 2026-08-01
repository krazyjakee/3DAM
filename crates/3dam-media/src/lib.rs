//! `dam-media` — format detection and the media-handler layer (tech-spec 04).
//!
//! Phase 2 (Media depth) fills in the cost-tiered contract (04 §4): **detection** stays cheap
//! (extension-ordered, with a couple of content sniffs), the **cheap tier** ([`extract_metadata`])
//! reads container/headers only, and the **expensive tier** ([`render_thumbnail`], the `convert`
//! encoders) fully decodes and is only ever called on preview/convert — never at ingest scale.
//! The audio/3D interactive previews are WASM islands (`dam-viewer`); only images produce a
//! server-rendered thumbnail here.

mod audio;
mod audio_features;
mod document;
mod features;
mod image;
mod mel;
mod model;
mod proc;
mod texture;
mod video;

pub use audio_features::{
    compute_waveform_peaks, decode_mono, extract_audio_features, AudioFeatures, LoopSource,
    WAVEFORM_BUCKETS,
};
pub use document::{extract_text, text_descriptor, MAX_TEXT_BYTES, TEXT_DIM};
pub use features::{extract_image_features, l2_normalise, ImageFeatures};
pub use mel::{log_mel, mel_from_samples, MelConfig, MelSpectrogram};
pub use video::probe_available as video_probe_available;

use dam_api::dto::{MediaAttributes, MediaType};
use std::path::Path;

/// What detection yields for a file: its media class and a concrete format tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detected {
    pub media: MediaType,
    /// Concrete format, lowercase (`wav`, `png`, `gltf`, …).
    pub format: String,
}

/// A per-asset handler fault. Always fail-soft at the call site (tech-spec 04 §6): the orchestrator
/// records it against the one asset and moves on; it never aborts a scan or a convert batch.
#[derive(Debug)]
pub enum HandlerError {
    /// Recognised format, but a sub-feature/codec this build does not support.
    Unsupported(String),
    /// Structurally invalid container/chunk.
    Corrupt(String),
    /// Failed to encode a derivative/output.
    Encode(String),
    Io(std::io::Error),
}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandlerError::Unsupported(s) => write!(f, "unsupported: {s}"),
            HandlerError::Corrupt(s) => write!(f, "corrupt: {s}"),
            HandlerError::Encode(s) => write!(f, "encode failed: {s}"),
            HandlerError::Io(e) => write!(f, "io: {e}"),
        }
    }
}
impl std::error::Error for HandlerError {}

/// A generated raster preview derivative (tech-spec 04 §6.4). PNG-encoded so the DOM shows it via
/// a plain `<img>`; the cache layer (02) stores `bytes` keyed by content hash.
#[derive(Clone, Debug)]
pub struct ThumbPng {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Lowercased final extension of a path, if any.
fn ext(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// Extensions that name a *container*, not a media type: an ISO-BMFF file may hold a video track,
/// or only audio. [`detect`] calls these video provisionally; [`refine_with_content`] settles them
/// against the real track table once bytes are available (ADR 0015).
const AMBIGUOUS_CONTAINERS: &[&str] = &["mp4", "mov", "m4v"];

/// The extension → (media, format) table for detection. Kept in one place so the format-coverage
/// matrix (tech-spec 04 §7) has a single source of truth.
///
/// Detection is **pure, cheap, and infallible**: extension in, classification out, with no
/// filesystem access. That is a requirement, not an optimisation — a scan calls this with the
/// source-relative *logical* path (a fetched remote file has a random local name), and it runs on
/// every walked entry including the progress pre-count, so it must not spawn a process or open a
/// file. Where the extension genuinely doesn't determine the answer, see [`refine_with_content`].
pub fn detect(path: &Path) -> Option<Detected> {
    let Some(e) = ext(path) else {
        // Extension-less licence files (`LICENSE`, `COPYING`, `NOTICE`) are the one case where the
        // *name* determines the type. They are also the dominant real-world spelling — bare
        // `LICENSE` is far more common than `LICENSE.txt` — so treating them as undetectable would
        // leave the licence surface with nothing to cite for most projects, which is one of the
        // motivations for cataloguing documents at all. They are plaintext; read them as such.
        return is_licence_evidence(path).then(|| Detected {
            media: MediaType::Document,
            format: "txt".to_string(),
        });
    };
    let media = match e.as_str() {
        // audio. `.m4a` is the audio-only ISO-BMFF extension by convention and is not probed —
        // an `.m4a` carrying a video track is a file that has already lied about itself.
        "wav" | "flac" | "mp3" | "ogg" | "oga" | "opus" | "aiff" | "aif" | "m4a" | "aac"
        | "wma" | "it" | "xm" | "mod" | "s3m" => MediaType::Audio,
        // image
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "tga" | "tiff" | "tif" | "webp" | "psd"
        | "exr" | "hdr" | "dds" | "ktx" | "ktx2" | "svg" => MediaType::Image,
        // 3d models
        "gltf" | "glb" | "fbx" | "obj" | "stl" | "ply" | "dae" | "3ds" | "blend" | "usd"
        | "usdz" | "usda" | "usdc" => MediaType::Model,
        // video. The shared containers provisionally classify as video here and are settled by
        // [`refine_with_content`] once real bytes exist — see that function for why.
        "mkv" | "webm" | "avi" | "ogv" => MediaType::Video,
        c if AMBIGUOUS_CONTAINERS.contains(&c) => MediaType::Video,
        // documents. `csv`/`json` are deliberately absent: they are structured data rather than
        // prose, and indexing them as documents would put machine output into a text index built
        // for language (PRODUCT_SPEC §9 phase 2b).
        "pdf" | "md" | "markdown" | "txt" | "rtf" | "docx" | "odt" => MediaType::Document,
        _ => return None,
    };
    // Normalise a few aliases to a canonical format tag.
    let format = match e.as_str() {
        "jpeg" => "jpg",
        "tif" => "tiff",
        "aif" => "aiff",
        "oga" => "ogg",
        "markdown" => "md",
        other => other,
    }
    .to_string();
    Some(Detected { media, format })
}

/// Path segments whose contents are dependencies, build output, or VCS internals rather than
/// project assets. Matched case-insensitively against whole path components.
const NOISE_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    ".svn",
    ".hg",
    "vendor",
    "target",
    "build",
    "dist",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".gradle",
    "cmakefiles",
    "obj",
    "bin",
    "library", // Unity's regenerated import cache
    "temp",
    "intermediate", // Unreal's build intermediates
    "saved",
    "deriveddatacache",
];

/// Detection **with the ingest ignore policy applied** — what a scan should use.
///
/// [`detect`] answers "what is this file?", which is a question about the file. This answers
/// "should the catalog hold it?", which is a question about the library, and the two are not the
/// same once documents are indexable. A single `npm install` puts thousands of `README.md` and
/// `LICENSE` files on disk; indexed naively they would outnumber a project's actual assets and
/// make the document media type worse than useless (PRODUCT_SPEC §9 phase 2b, risk 4).
///
/// The policy is deliberately narrow:
/// - It only ever filters **documents**. Real assets are never dropped, whatever directory they
///   are in — a texture under `build/` is still a texture, and silently omitting it would be a far
///   worse failure than over-indexing a readme.
/// - Documents are dropped when they sit under a dependency/build/VCS directory. That is where
///   generated and third-party noise lives, and nothing there is authored by the project.
/// - **Except** licence evidence, at any depth. A purchased asset pack unpacked into `vendor/` puts
///   its `LICENSE.txt` under a noise directory, and that file is precisely what the licence surface
///   has to point at — dropping it would silently remove the evidence for a licence claim, which is
///   a worse failure than indexing a few dependency readmes. So [`LICENCE_EVIDENCE`] names are kept
///   wherever they are.
pub fn detect_for_ingest(rel_path: &Path) -> Option<Detected> {
    let det = detect(rel_path)?;
    if det.media != MediaType::Document {
        return Some(det);
    }
    let in_noise_dir = rel_path
        .parent()
        .into_iter()
        .flat_map(|p| p.components())
        .filter_map(|c| c.as_os_str().to_str())
        .any(|seg| NOISE_DIRS.contains(&seg.to_ascii_lowercase().as_str()));
    (!in_noise_dir || is_licence_evidence(rel_path)).then_some(det)
}

/// Filename stems that carry licence/attribution evidence. Matched case-insensitively against the
/// first word of the filename, so `LICENSE`, `LICENSE.txt` and `LICENCE-MIT.md` all qualify.
///
/// `readme` is deliberately **not** here. A readme sometimes mentions licence terms, but it is also
/// the single highest-volume filename in any dependency tree — one `npm install` is thousands of
/// them — so admitting it would undo the very policy this exception is carved out of and drown the
/// catalog (the risk the epic flags). Licence files are far rarer and are the actual evidence the
/// licence surface needs to cite, so the exception stays narrow enough to be worth its cost.
const LICENCE_EVIDENCE: &[&str] = &["license", "licence", "copying", "notice", "eula"];

fn is_licence_evidence(rel_path: &Path) -> bool {
    let name = rel_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let base = name.split(['.', '-', '_']).next().unwrap_or_default();
    LICENCE_EVIDENCE.contains(&base.to_ascii_lowercase().as_str())
}

/// Settle a provisional [`detect`] result against the file's actual bytes.
///
/// `detect` classifies by extension because that is all a scan has at walk time — the logical path.
/// (A remote source's fetched temp file has a random name, which is exactly why detection can't use
/// the local path.) But `.mp4`, `.mov` and `.m4v` name a *container*, not a media type: the same
/// extension covers a cutscene and an audio-only file, and `mp4` is already in the audio decode
/// matrix. Calling one wrong means the wrong badge, the wrong preview, and a poster-frame render
/// that can never succeed.
///
/// So the ambiguity is resolved here instead, once, at the one point in the scan where real bytes
/// are on local disk. Returns `Some(corrected)` only when the classification actually changes, so
/// the caller can cheaply skip the common case.
///
/// With no prober installed this returns `None` and the provisional answer stands, which is the
/// documented ADR 0015 fallback: `.mp4`/`.mov`/`.m4v` are overwhelmingly video, and mis-typing the
/// rare audio-only one costs a badge, not a broken asset — the bytes still play.
pub fn refine_with_content(det: &Detected, abs: &Path) -> Option<Detected> {
    if !AMBIGUOUS_CONTAINERS.contains(&det.format.as_str()) {
        return None;
    }
    match video::has_video_stream(abs) {
        Some(false) => Some(Detected {
            media: MediaType::Audio,
            format: det.format.clone(),
        }),
        _ => None,
    }
}

/// CHEAP tier (tech-spec 04 §4): read the media-specific attribute struct from container headers /
/// metadata only — no geometry decode, no PCM decode, no full pixel decode, no GPU. Runs on every
/// asset at ingest. Best-effort and fail-soft: a partial or empty struct is returned on fault,
/// never an error that would sink the scan.
pub fn extract_metadata(path: &Path, det: &Detected) -> MediaAttributes {
    match det.media {
        MediaType::Audio => MediaAttributes::Audio(audio::metadata(path, &det.format)),
        MediaType::Image => MediaAttributes::Image(image::metadata(path, &det.format)),
        MediaType::Model => MediaAttributes::Model(model::metadata(path, &det.format)),
        MediaType::Video => MediaAttributes::Video(video::metadata(path, &det.format)),
        MediaType::Document => MediaAttributes::Document(document::metadata(path, &det.format)),
    }
}

/// EXPENSIVE tier: render a downscaled PNG thumbnail (tech-spec 04 §6.4). Images decode directly;
/// video produces a poster frame via a discovered ffmpeg (ADR 0015). Audio waveforms and 3D
/// turntables are interactive WASM islands and documents render an excerpt card in the DOM, so
/// this returns `Unsupported` for them and the UI falls back to the honest typed tile.
pub fn render_thumbnail(
    path: &Path,
    det: &Detected,
    max_edge: u32,
) -> Result<ThumbPng, HandlerError> {
    match det.media {
        MediaType::Image => {
            // The format is what routes a DDS/KTX2 to the texture decoder (issue #49); the raster
            // path ignores it.
            let (bytes, width, height) = image::thumbnail(path, max_edge, &det.format)?;
            Ok(ThumbPng {
                bytes,
                width,
                height,
            })
        }
        MediaType::Video => {
            let (bytes, width, height) = video::thumbnail(path, max_edge)?;
            Ok(ThumbPng {
                bytes,
                width,
                height,
            })
        }
        // A document's "thumbnail" is its opening text, which the client already has as
        // `DocumentAttributes::excerpt` and can render with real fonts, real theming, and
        // selectable text. Rasterising type server-side would need a font stack and a PDF
        // renderer — the native-dependency class ADR 0015 declined for video — to produce a
        // strictly worse tile.
        MediaType::Audio | MediaType::Model | MediaType::Document => {
            Err(HandlerError::Unsupported(format!(
                "{} previews render client-side, not as a server thumbnail",
                det.media.as_str()
            )))
        }
    }
}

/// EXPENSIVE tier: decode + re-encode an image to a raster target (convert pipeline, tech-spec 08
/// §3.2). Returns the encoded bytes.
pub fn convert_image(
    path: &Path,
    target_format: &str,
    max_edge: Option<u32>,
    quality: Option<u8>,
) -> Result<Vec<u8>, HandlerError> {
    image::convert(path, target_format, max_edge, quality)
}

/// EXPENSIVE tier: decode audio and encode a canonical 16-bit WAV (convert pipeline, tech-spec 08
/// §3.1). Returns the encoded bytes.
pub fn convert_audio(
    path: &Path,
    source_format: &str,
    target_format: &str,
) -> Result<Vec<u8>, HandlerError> {
    if target_format != "wav" {
        return Err(HandlerError::Unsupported(format!(
            "audio target {target_format:?} is not supported in this build (v1 encodes WAV)"
        )));
    }
    let mut cursor = std::io::Cursor::new(Vec::new());
    audio::convert_to_wav(path, source_format, &mut cursor)?;
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_api::dto::MediaAttributes;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    fn tmp(name: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("3dam-media-test-{}-{n}-{name}", std::process::id()))
    }

    fn det(media: MediaType, format: &str) -> Detected {
        Detected {
            media,
            format: format.to_string(),
        }
    }

    /// A 4x4 BC1 DDS whose single block is solid red — the raster equivalent of a 4x4 red PNG,
    /// which is what makes the two analysable side by side. `c0 = 0xF800` is red in RGB565; note
    /// `0xFFFF` there would be *white*, which is an easy fixture mistake to make.
    fn red_bc1_dds() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"DDS ");
        let mut h = [0u32; 31];
        h[0] = 124;
        h[1] = 0x1 | 0x2 | 0x4 | 0x1000 | 0x80000;
        h[2] = 4;
        h[3] = 4;
        h[4] = 8;
        h[6] = 1;
        h[18] = 32;
        h[19] = 0x4;
        h[20] = u32::from_le_bytes(*b"DXT1");
        h[26] = 0x1000;
        for w in h {
            v.extend_from_slice(&w.to_le_bytes());
        }
        v.extend_from_slice(&0xF800u16.to_le_bytes());
        v.extend_from_slice(&0x001Fu16.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v
    }

    fn write_png(path: &Path, w: u32, h: u32) {
        // `::image` — the crate, not this crate's `mod image` which shadows the bare name here.
        let img = ::image::RgbaImage::from_pixel(w, h, ::image::Rgba([10, 20, 30, 255]));
        img.save_with_format(path, ::image::ImageFormat::Png)
            .unwrap();
    }

    #[test]
    fn detects_video_and_document_extensions() {
        let media_of = |p: &str| detect(Path::new(p)).map(|d| d.media);
        // Unambiguous video containers need no probe.
        for p in ["a.mkv", "a.webm", "a.avi", "a.ogv"] {
            assert_eq!(media_of(p), Some(MediaType::Video), "{p}");
        }
        for p in ["a.pdf", "a.md", "a.txt", "a.rtf", "a.docx", "a.odt"] {
            assert_eq!(media_of(p), Some(MediaType::Document), "{p}");
        }
        // `.m4a` is audio by convention and must not be probed into video.
        assert_eq!(media_of("a.m4a"), Some(MediaType::Audio));
        // Structured data is deliberately *not* a document (PRODUCT_SPEC §9 phase 2b).
        assert_eq!(media_of("a.csv"), None);
        assert_eq!(media_of("a.json"), None);
        // Alias normalisation.
        assert_eq!(
            detect(Path::new("a.markdown")).map(|d| d.format),
            Some("md".to_string())
        );
    }

    #[test]
    fn extensionless_licence_files_are_documents() {
        // The dominant real-world spelling has no extension at all, and the licence surface has to
        // be able to cite it. Read as plaintext.
        for p in [
            "LICENSE",
            "COPYING",
            "NOTICE",
            "licence",
            "LICENSE-MIT",
            "EULA",
        ] {
            let d = detect(Path::new(p)).unwrap_or_else(|| panic!("{p} should be detected"));
            assert_eq!(d.media, MediaType::Document, "{p}");
            assert_eq!(d.format, "txt", "{p}");
        }
        // The name is only load-bearing when there is no extension — this must not become a
        // general "sniff every extension-less file" rule.
        assert_eq!(detect(Path::new("Makefile")), None);
        assert_eq!(detect(Path::new("some-binary")), None);
        assert_eq!(detect(Path::new("CHANGELOG")), None);
        // And an extension still wins where present.
        assert_eq!(
            detect(Path::new("LICENSE.md")).map(|d| d.format),
            Some("md".to_string())
        );
    }

    #[test]
    fn ambiguous_containers_default_to_video_and_refine_is_a_no_op_without_bytes() {
        // `detect` is extension-only and infallible — it must never touch the filesystem, because
        // during a scan it is handed a *relative* logical path (a fetched remote file has a random
        // local name). So the shared containers classify provisionally as video here...
        for p in ["a.mp4", "a.mov", "a.m4v"] {
            assert_eq!(
                detect(Path::new(p)).map(|d| d.media),
                Some(MediaType::Video),
                "{p}"
            );
        }
        // ...and refinement, given no readable bytes (or no prober), leaves that answer alone —
        // ADR 0015's documented fallback rather than a drop or a guess of audio.
        let det = detect(Path::new("a.mp4")).unwrap();
        assert!(refine_with_content(&det, Path::new("/nonexistent/a.mp4")).is_none());

        // Refinement only ever considers the ambiguous containers; everything else short-circuits
        // without so much as a stat, whatever path it is handed.
        for fmt in ["png", "wav", "glb", "mkv", "webm", "pdf"] {
            let det = Detected {
                media: MediaType::Image,
                format: fmt.to_string(),
            };
            assert!(
                refine_with_content(&det, Path::new("/nonexistent/x")).is_none(),
                "{fmt} must not be probed"
            );
        }
    }

    #[test]
    fn ingest_policy_drops_dependency_documents_only() {
        let ingest = |p: &str| detect_for_ingest(Path::new(p)).map(|d| d.media);

        // Project documents survive — including the root LICENSE the licence surface points at.
        assert_eq!(ingest("LICENSE.txt"), Some(MediaType::Document));
        assert_eq!(ingest("packs/kenney/README.md"), Some(MediaType::Document));
        assert_eq!(ingest("docs/design/combat.pdf"), Some(MediaType::Document));

        // Dependency/build/VCS noise does not.
        assert_eq!(ingest("node_modules/left-pad/README.md"), None);
        assert_eq!(ingest("target/debug/notes.txt"), None);
        assert_eq!(ingest(".git/COMMIT_EDITMSG.txt"), None);
        // Case-insensitive on the directory segment.
        assert_eq!(ingest("Build/readme.md"), None);

        // …except licence evidence, which survives at any depth: an asset pack unpacked into
        // `vendor/` still has to be able to answer "what am I allowed to do with this?".
        assert_eq!(
            ingest("vendor/kenney-pack/LICENSE.txt"),
            Some(MediaType::Document),
            "licence evidence must survive a noise directory — it's what the licence surface cites"
        );
        assert_eq!(
            ingest("web/node_modules/x/LICENCE-MIT.md"),
            Some(MediaType::Document)
        );
        assert_eq!(ingest("target/COPYING"), Some(MediaType::Document));
        // But a readme is not licence evidence, and it is the highest-volume filename in a
        // dependency tree — admitting it would undo the policy (asserted above, and again here at
        // depth) rather than narrowly carve out of it.
        assert_eq!(ingest("vendor/pack/readme.md"), None);

        // Real assets are never dropped, wherever they live — a texture under build/ is a texture.
        assert_eq!(
            ingest("node_modules/pack/tex.png"),
            Some(MediaType::Image),
            "the policy must only ever filter documents"
        );
        assert_eq!(ingest("target/release/kick.wav"), Some(MediaType::Audio));
        assert_eq!(ingest("build/prop.glb"), Some(MediaType::Model));
    }

    #[test]
    fn document_metadata_flows_through_extract_metadata() {
        let p = tmp("notes.md");
        std::fs::write(&p, "# Title\n\nsome body words here\n").unwrap();
        let MediaAttributes::Document(d) = extract_metadata(&p, &det(MediaType::Document, "md"))
        else {
            panic!("expected document attrs")
        };
        assert_eq!(d.title.as_deref(), Some("Title"));
        assert!(d.word_count.unwrap() > 0);
        assert!(d.excerpt.is_some());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn documents_have_no_server_thumbnail() {
        // The excerpt card is DOM-rendered; the server must say so rather than produce a raster.
        let p = tmp("doc.txt");
        std::fs::write(&p, "hello").unwrap();
        let err = render_thumbnail(&p, &det(MediaType::Document, "txt"), 64).unwrap_err();
        assert!(matches!(err, HandlerError::Unsupported(_)));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn image_metadata_reads_dimensions_and_alpha() {
        let p = tmp("dims.png");
        write_png(&p, 64, 48);
        let attrs = extract_metadata(&p, &det(MediaType::Image, "png"));
        let MediaAttributes::Image(i) = attrs else {
            panic!("expected image attrs")
        };
        assert_eq!(i.width, Some(64));
        assert_eq!(i.height, Some(48));
        assert_eq!(i.has_alpha, Some(true)); // RGBA PNG → colour type 6
        assert_eq!(i.color_space.as_deref(), Some("srgb"));
        std::fs::remove_file(&p).ok();
    }

    /// A GPU texture must reach the texture decoder through the *dispatch*, not just in isolation.
    /// The routing key is `det.format`; before issue #49 this call handed a DDS to the raster
    /// decoder, which cannot open one, so the preview came back as an error.
    #[test]
    fn a_dds_thumbnail_routes_to_the_texture_decoder() {
        let p = tmp("dispatch.dds");
        std::fs::write(&p, red_bc1_dds()).unwrap();

        let thumb = render_thumbnail(&p, &det(MediaType::Image, "dds"), 64).unwrap();
        assert!(thumb.bytes.starts_with(b"\x89PNG"), "PNG-encoded");
        // Not an exact size assertion: `thumbnail` fits the long edge to the box, and this
        // deliberately tiny 4x4 fixture is scaled *up* to it — same as any small raster.
        assert!(thumb.width <= 64 && thumb.height <= 64);
        // The point of the test: real decoded pixels came back, not a blank surface. The block is
        // solid red, so a thumbnail that routed to the raster decoder (or produced nothing) fails.
        let decoded = ::image::load_from_memory(&thumb.bytes).unwrap().to_rgba8();
        let px = decoded
            .get_pixel(decoded.width() / 2, decoded.height() / 2)
            .0;
        assert!(
            px[0] > 200 && px[1] < 60 && px[2] < 60,
            "expected the texture's red, got {px:?}"
        );

        // …and the cheap tier answers through the same dispatch.
        let attrs = extract_metadata(&p, &det(MediaType::Image, "dds"));
        match attrs {
            MediaAttributes::Image(i) => {
                assert_eq!(i.texture_format.as_deref(), Some("BC1_UNORM"));
                assert_eq!((i.width, i.height), (Some(4), Some(4)));
            }
            other => panic!("expected image attributes, got {other:?}"),
        }
        // …and the expensive tier reaches them everywhere, not just for thumbnails. Analysis and
        // convert both decode through the same routed entry point, so a texture participates in
        // similarity/dedup/auto-tag rather than being a preview-only catalog row.
        let feats = extract_image_features(&p).expect("a texture must be analysable");
        // Not `phash != 0`: a dHash of a *uniform* image is legitimately all-zero (no gradients),
        // and this fixture is solid red. The embedding is the signal that matters — without one a
        // texture can never appear as a similarity or dedup result.
        assert!(
            !feats.embedding.is_empty(),
            "a texture must get an embedding, or it can never be a similarity result"
        );
        assert!(
            !feats.dominant_colors.is_empty(),
            "a texture must get dominant colours, or auto-tagging cannot see it"
        );
        let png = convert_image(&p, "png", None, None).expect("dds → png must convert");
        assert!(png.starts_with(b"\x89PNG"));

        let _ = std::fs::remove_file(&p);
    }

    /// A texture and an ordinary raster of the same content must analyse to the same answer.
    ///
    /// This is the assertion that a routed decode is *equivalent*, not merely non-empty — the
    /// analysis pass feeds similarity, dedup and auto-tagging, so a texture that decodes to
    /// different pixels than its PNG twin would quietly sort into different neighbourhoods.
    #[test]
    fn a_texture_and_an_equivalent_png_analyse_alike() {
        let png = tmp("solid.png");
        ::image::RgbaImage::from_pixel(4, 4, ::image::Rgba([255, 0, 0, 255]))
            .save_with_format(&png, ::image::ImageFormat::Png)
            .unwrap();
        let from_png = extract_image_features(&png).unwrap();
        let _ = std::fs::remove_file(&png);

        let dds = tmp("solid_equiv.dds");
        std::fs::write(&dds, red_bc1_dds()).unwrap();
        let from_dds = extract_image_features(&dds).unwrap();
        let _ = std::fs::remove_file(&dds);

        assert_eq!(from_dds.dominant_colors, from_png.dominant_colors);
        assert_eq!(from_dds.phash, from_png.phash);
    }

    #[test]
    fn image_thumbnail_and_convert() {
        let p = tmp("src.png");
        write_png(&p, 200, 100);
        let thumb = render_thumbnail(&p, &det(MediaType::Image, "png"), 64).unwrap();
        assert!(thumb.width <= 64 && thumb.height <= 64);
        assert!(thumb.bytes.starts_with(b"\x89PNG"), "PNG-encoded");
        // Aspect preserved: 200x100 → long edge 64 → 64x32.
        assert_eq!((thumb.width, thumb.height), (64, 32));

        let jpg = convert_image(&p, "jpg", Some(50), Some(80)).unwrap();
        assert!(jpg.starts_with(&[0xFF, 0xD8]), "JPEG SOI marker");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn gltf_json_counts_and_flags() {
        // One mesh, one primitive: POSITION accessor count 3, indices accessor count 3 (mode
        // default TRIANGLES) → 3 verts, 1 triangle. TEXCOORD_0 present → has_uvs.
        let doc = r#"{
          "meshes":[{"primitives":[{"attributes":{"POSITION":0,"TEXCOORD_0":1},"indices":2}]}],
          "accessors":[{"count":3},{"count":3},{"count":3}],
          "materials":[{}],
          "textures":[{}],
          "skins":[{}],
          "animations":[{}]
        }"#;
        let p = tmp("m.gltf");
        std::fs::write(&p, doc).unwrap();
        let MediaAttributes::Model(m) = extract_metadata(&p, &det(MediaType::Model, "gltf")) else {
            panic!("expected model attrs")
        };
        assert_eq!(m.vertex_count, Some(3));
        assert_eq!(m.triangle_count, Some(1));
        assert_eq!(m.mesh_count, Some(1));
        assert_eq!(m.material_count, Some(1));
        assert_eq!(m.texture_count, Some(1));
        assert_eq!(m.has_rig, Some(true));
        assert_eq!(m.has_animation, Some(true));
        assert_eq!(m.has_uvs, Some(true));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn glb_container_json_chunk() {
        let mut json =
            br#"{"meshes":[{"primitives":[{"attributes":{"POSITION":0},"indices":1}]}],"accessors":[{"count":24},{"count":36}]}"#
                .to_vec();
        while !json.len().is_multiple_of(4) {
            json.push(b' '); // chunks are 4-byte aligned
        }
        let mut glb = Vec::new();
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes()); // version
        glb.extend_from_slice(&((12 + 8 + json.len()) as u32).to_le_bytes()); // total length
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes()); // chunk length
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);
        let p = tmp("m.glb");
        std::fs::write(&p, &glb).unwrap();
        let MediaAttributes::Model(m) = extract_metadata(&p, &det(MediaType::Model, "glb")) else {
            panic!("expected model attrs")
        };
        assert_eq!(m.vertex_count, Some(24));
        assert_eq!(m.triangle_count, Some(12)); // 36 indices / 3
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn obj_stl_ply_counts() {
        let obj = tmp("c.obj");
        std::fs::write(&obj, "v 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nf 1 2 3\n").unwrap();
        let MediaAttributes::Model(m) = extract_metadata(&obj, &det(MediaType::Model, "obj"))
        else {
            panic!()
        };
        assert_eq!(m.vertex_count, Some(3));
        assert_eq!(m.triangle_count, Some(1));
        assert_eq!(m.has_uvs, Some(true));

        let stl = tmp("c.stl");
        std::fs::write(
            &stl,
            "solid t\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nendloop\nendfacet\nendsolid t\n",
        )
        .unwrap();
        let MediaAttributes::Model(m) = extract_metadata(&stl, &det(MediaType::Model, "stl"))
        else {
            panic!()
        };
        assert_eq!(m.triangle_count, Some(1));

        let ply = tmp("c.ply");
        std::fs::write(
            &ply,
            "ply\nformat ascii 1.0\nelement vertex 8\nproperty float x\nelement face 12\nproperty list uchar int vertex_index\nend_header\n",
        )
        .unwrap();
        let MediaAttributes::Model(m) = extract_metadata(&ply, &det(MediaType::Model, "ply"))
        else {
            panic!()
        };
        assert_eq!(m.vertex_count, Some(8));
        assert_eq!(m.triangle_count, Some(12));
        std::fs::remove_file(&obj).ok();
        std::fs::remove_file(&stl).ok();
        std::fs::remove_file(&ply).ok();
    }

    #[test]
    fn audio_metadata_and_wav_convert() {
        // Synthesize a 1-second mono 8 kHz WAV with hound, then probe it back.
        let p = tmp("tone.wav");
        {
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 8000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut w = hound::WavWriter::create(&p, spec).unwrap();
            for _ in 0..8000 {
                w.write_sample(0i16).unwrap();
            }
            w.finalize().unwrap();
        }
        let MediaAttributes::Audio(a) = extract_metadata(&p, &det(MediaType::Audio, "wav")) else {
            panic!("expected audio attrs")
        };
        assert_eq!(a.sample_rate, Some(8000));
        assert_eq!(a.channels, Some(1));
        assert_eq!(a.duration_ms, Some(1000));

        let wav = convert_audio(&p, "wav", "wav").unwrap();
        assert!(wav.starts_with(b"RIFF"), "WAV RIFF header");
        assert!(
            convert_audio(&p, "wav", "mp3").is_err(),
            "non-WAV target rejected"
        );
        std::fs::remove_file(&p).ok();
    }
}
