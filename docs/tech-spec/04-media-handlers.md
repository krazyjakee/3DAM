# 04 — Media handlers

Status: **Draft v0.1** · Scope: the `MediaHandler` plugin layer — detection, decode, cost-tiered metadata/thumbnail/feature extraction, and the per-media format matrix.

This file specifies the **media-handler seam**: the trait every media type (audio, image, 3D)
implements, how a handler is selected for a file by sniffing content rather than trusting an
extension, and the **cost-tiered contract** that keeps ingest cheap. It is the low-level design
under [PRODUCT_SPEC](../PRODUCT_SPEC.md) §5 (media-specific attributes), §6.2 (the cost-tiered
extraction rule), and §6.4 (preview), and under [DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §2
(media plugins) and §3.2 (previews).

It owns the trait, detection, cheap metadata extraction, and the format matrix. It does **not**
own the actual wgpu render (that is [06-3d-render.md](06-3d-render.md)), embeddings /
similarity / dedup / auto-tag (that is [05-analysis-similarity-dedup.md](05-analysis-similarity-dedup.md)),
or the convert/optimise pipeline (that is [08-convert-pipeline.md](08-convert-pipeline.md)). This
file defines the *inputs and outputs* at those seams and links across; it fills in mechanics,
it does not re-decide product rationale or ADRs.

**Depends on:** the asset/attribute schema ([02-data-model-and-storage.md](02-data-model-and-storage.md))
for where these structs persist; the `LibraryService` error model ([03-library-service-and-api.md](03-library-service-and-api.md))
for how a handler failure becomes a fail-soft per-asset error, not a scan abort.

---

## 1. Where handlers sit

A handler is a **pure, stateless transformer** of bytes into structured facts and derivatives.
It knows nothing about the database, the source layer, sources, jobs, or transports. `3dam-core`
owns a **handler registry**; scanning, analysis, and preview orchestration call *into* handlers
and persist what comes back.

```
   scan/analysis/preview orchestration  (3dam-core — owns concurrency, persistence, fail-soft)
                    │
                    ▼   selects one handler by sniffing bytes
        ┌────────────────────────────┐
        │      MediaHandler (04)      │   detect · decode · extract_metadata
        │  audio · image · 3d impls   │   · thumbnail · extract_features
        └────────────────────────────┘
             │            │
             ▼            ▼
        raw format     3dam-render (06)  ← 3D thumbnail/multiview delegates here
        crates         + analysis (05)   ← extract_features feeds embeddings here
```

The handler produces data; the caller decides *when* to call each method (ingest vs preview vs
analysis) and enforces the cost tiers below. Nothing in a handler blocks on I/O beyond the reader
it is handed, and nothing in a handler touches the GPU directly — the 3D handler delegates render
work to `3dam-render` (06).

---

## 2. The `MediaHandler` trait

Rust-ish pseudocode; indicative, not a frozen API (see [00-overview.md](00-overview.md) conventions).

### 2.1 Shared types

```rust
/// The media types. Audio/Image/Model are the three *deep* ones this spec is written around;
/// Video and Document (PRODUCT_SPEC §9 phase 2b) are deliberately shallower — see §7.4/§7.5.
/// The concrete format is a separate string/enum on the asset.
pub enum MediaType { Audio, Image, Model, Video, Document }

/// Result of sniffing: which media type + concrete format a byte stream is.
pub struct FormatId {
    pub media: MediaType,
    pub format: &'static str,   // "wav", "png", "glb", "fbx", "obj", ...
    pub confidence: Confidence, // Magic | Container | ExtensionOnly
}

pub enum Confidence { Magic, Container, ExtensionOnly }

/// A cheap, seekable byte source. Handlers read through this; the caller owns the
/// underlying file/network handle and the read budget (§4). Sources (07) that cannot
/// seek are buffered by the caller before a handler sees them.
pub trait AssetReader: Read + Seek + Send {}

/// Everything a handler may need about the file it did not read itself.
pub struct AssetInput<'a> {
    pub reader: &'a mut dyn AssetReader,
    pub hint_ext: Option<&'a str>,   // filename extension, a hint only — never trusted alone
    pub declared_size: Option<u64>,  // from the source listing, if known
}

/// Per-media attribute payloads (§5 of the product spec). One variant per media type.
/// These are the *cheap-tier* outputs — the CHEAP contract (§4) is what fills them.
pub enum MediaAttributes {
    Audio(AudioAttributes),
    Image(ImageAttributes),
    Model(ModelAttributes),
}

/// A decoded, in-memory representation, media-specific. Only produced on demand
/// (preview/analysis/convert), never at ingest.
pub enum Decoded {
    Audio(DecodedAudio),   // PCM frames + spec
    Image(DecodedImage),   // pixel buffer + colour space
    Model(DecodedModel),   // meshes, materials, node graph (full gltf/fbxcel decode)
}

/// A generated preview derivative, tagged so the caller can cache/store it (02).
pub struct Thumbnail {
    pub kind: ThumbKind,      // Png | WaveformPng | (3D turntable is Png from 06)
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,       // encoded (PNG); waveform peaks may be a compact peak file
}

/// Opaque feature payload handed to analysis (05) — raw material for embeddings,
/// perceptual hashes, spectral descriptors, tileability, multi-view render sets.
/// This file defines that handlers *produce* it; 05 defines what it *becomes*.
pub struct FeatureBundle {
    pub extractor_versions: Vec<(&'static str, u32)>, // for re-analysis gating (05)
    pub payload: FeaturePayload,                       // media-specific, defined per handler
}
```

### 2.2 The trait

```rust
pub trait MediaHandler: Send + Sync {
    /// Which media type this handler serves. Used to shard the registry (§3).
    fn media_type(&self) -> MediaType;

    /// CHEAP. Sniff the leading bytes / container structure and report whether this
    /// handler claims the stream, and as which concrete format. Reads only a small,
    /// bounded prefix (and may seek to a trailer for a few formats). MUST NOT decode
    /// payload, allocate large buffers, or touch the GPU. Returns None if unclaimed.
    fn detect(&self, input: &mut AssetInput) -> Result<Option<FormatId>, HandlerError>;

    /// CHEAP tier. Extract the media-specific attribute struct (§5) from container
    /// headers / metadata chunks ONLY. No geometry decode, no full audio decode, no
    /// pixel decode beyond the header, no GPU. This is what runs on every asset at
    /// ingest scale — see the cost contract (§4).
    fn extract_metadata(&self, input: &mut AssetInput, fmt: &FormatId)
        -> Result<MediaAttributes, HandlerError>;

    /// EXPENSIVE tier. Fully decode into an in-memory representation. Called for
    /// preview, feature extraction, and convert (08) — never at ingest. May allocate
    /// large buffers (vertex data, PCM, pixels).
    fn decode(&self, input: &mut AssetInput, fmt: &FormatId)
        -> Result<Decoded, HandlerError>;

    /// EXPENSIVE tier. Produce a preview derivative (§6.4). Image: downscaled PNG.
    /// Audio: waveform peaks / waveform PNG. 3D: delegates to 3dam-render (06) for a
    /// turntable/still — the handler supplies the decoded mesh + deterministic framing,
    /// 06 owns the wgpu path and the software-raster fallback. May take a pre-`decode`d
    /// value to avoid decoding twice.
    fn thumbnail(&self, decoded: &Decoded, req: ThumbRequest)
        -> Result<Thumbnail, HandlerError>;

    /// EXPENSIVE tier. Produce the raw feature material for analysis (05): image
    /// perceptual hash + colour + tileability inputs + CLIP-ready pixels; audio
    /// spectral/temporal descriptors + embedding-ready frames; 3D geometry-derived
    /// features + the multi-view render set (rendered via 06). This file defines the
    /// *output type*; 05 owns embeddings, ANN, dedup, and auto-tag.
    fn extract_features(&self, decoded: &Decoded, req: FeatureRequest)
        -> Result<FeatureBundle, HandlerError>;
}

/// A handler failure is always per-asset and fail-soft (DESIGN_GUIDELINES §2). The
/// caller records it against the one asset and continues the scan — never aborts it.
pub enum HandlerError {
    Unrecognised,                 // detect() found nothing it owns
    Truncated { at: u64 },        // stream ended mid-structure
    Corrupt { detail: String },   // structurally invalid container/chunk
    Unsupported { detail: String},// recognised format, unsupported sub-feature/codec
    Io(std::io::Error),
}
```

**Why the split.** `detect` + `extract_metadata` are the **CHEAP tier**; `decode` + `thumbnail`
+ `extract_features` are the **EXPENSIVE tier**. The trait is split on this line precisely so the
scanner can run the cheap tier across a million assets at ingest and *defer* every expensive method
to when the user opens or the analysis pipeline reaches an asset (PRODUCT_SPEC §6.2). Keeping
`extract_metadata` a separate method — rather than a mode flag on `decode` — makes the cheap
contract enforceable and testable: a cheap-tier method that reaches for a vertex buffer or a GPU
is a bug, not a slow path.

---

## 3. Format detection & handler selection

Selection is **content-first**, never extension-first (DESIGN_GUIDELINES §2 "detect"; PRODUCT_SPEC
§4.3 "detect"). The extension is a *hint* used only to order the sniff and as a last-resort
tie-break, never as the sole decider.

### 3.1 The registry

`3dam-core` holds a `HandlerRegistry` — a set of `MediaHandler` implementations, one (or more) per
media type, plus a small **sniff table** mapping magic signatures and container markers to a
handler + candidate format. Registration is compile-time via feature gates
([01-architecture-and-crates.md](01-architecture-and-crates.md)); a disabled format simply is not
in the table.

### 3.2 The sniff flow

```
select_handler(input):
  1. peek up to N bytes (N ≈ 64 for magic; a few formats need a trailer seek)
  2. match the prefix against the sniff table:
       - exact magic  → FormatId{ confidence: Magic }        e.g. "RIFF..WAVE", 0x89PNG,
                                                              "glTF"(0x46546C67), "OggS", ID3
       - container probe → ask the owning handler's detect()  e.g. RIFF subtype, ISOBMFF ftyp,
                                confidence: Container          gltf JSON vs glb binary, FBX magic
  3. if the prefix is ambiguous, consult hint_ext to ORDER remaining candidates,
     then call each candidate handler.detect() until one claims it (Magic/Container)
  4. if nothing claims it but hint_ext maps to exactly one handler, offer it that
     stream at ExtensionOnly confidence; the handler MAY still reject in detect()
  5. nothing claims it → HandlerError::Unrecognised → asset catalogued as "unknown
     format", fail-soft (§6), never dropped from the library
```

Key points:

- **Magic bytes / container sniff win over extension.** A `.wav` that is really an Ogg stream is
  detected as Ogg; a `.txt` that is really a PNG is detected as PNG. A mismatched extension is
  recorded (so it can surface as a data-quality flag) but does not change the handler chosen.
- **`detect` is bounded and side-effect-free.** It reads a small prefix (and, for the few formats
  that carry data in a trailer, one seek to the end), then rewinds. It never decodes payload.
- **`ExtensionOnly` confidence is degraded, not trusted.** Extension-only claims are marked so
  the UI/CLI can surface "format inferred from name" and analysis can be re-run if a better sniff
  path lands.
- **OBJ and other text formats** have no reliable magic. Their handler's `detect` does a *bounded*
  structural probe (first non-comment tokens look like `v`/`vn`/`vt`/`f`/`mtllib`), gated behind the
  extension hint to avoid scanning every text file — still content-checked, just extension-ordered.

### 3.3 Selection output

`select_handler` returns `(FormatId, &dyn MediaHandler)`. The `FormatId` (media type + concrete
format + confidence) is persisted on the asset (02); the concrete-format string is what the format
matrix (§7) and the convert pipeline (08) key off.

---

## 4. The cost-tiered contract

This is the load-bearing rule of this file. It makes PRODUCT_SPEC §6.2 concrete and enforceable.

| Tier | Methods | May touch | MUST NOT touch | When it runs |
|---|---|---|---|---|
| **CHEAP** | `detect`, `extract_metadata` | container/header/metadata chunks; a bounded byte prefix; trailer seek; accessor/chunk *counts* | geometry decode; full audio/PCM decode; full pixel decode; large allocations; the GPU; network beyond the given reader | on **every asset at ingest**, at scan scale (1M+) |
| **EXPENSIVE** | `decode`, `thumbnail`, `extract_features` | full decode; large buffers; CPU DSP/FFT; the GPU (3D, via 06) | — (this is where the cost lives) | **deferred** to preview open / the analysis pipeline (05) |

### 4.1 What CHEAP is allowed to compute

- **Audio:** duration, sample rate, bit depth, channel count, codec — all from the container/format
  header (RIFF `fmt ` chunk, Ogg/FLAC headers, ID3/MP4 metadata). **No PCM is decoded.** BPM, key,
  loudness, and spectral descriptors are *expensive* and belong to `extract_features` → 05.
- **Image:** dimensions, colour depth, alpha presence, colour space, embedded ICC/format metadata —
  from the header only. **No pixels are decoded.** Dominant colours, perceptual hash, tileability,
  and type classification are *expensive* (`extract_features` → 05), with the single documented
  exception that the tileability edge-test is cheap enough to run at ingest **on a thumbnail** and is
  therefore treated as a preview-tier feature, not a header field (PRODUCT_SPEC §5 tileability).
- **3D:** the two-tier GLB read (`../3d-handler-notes.md` §1) is the canonical CHEAP example. The
  handler **hand-walks the GLB container** — 12-byte header, JSON chunk parsed as a value, BIN chunk
  **skipped** — and reads node/mesh/material/accessor/skin counts, per-node mesh+skin refs (→
  `has_rig`/`has_animation`), UV accessor presence, and a bbox from accessor min/max. Exact triangle
  counts come from accessor `count` fields **in the JSON chunk** — so even precise geometry stats
  need **no BIN decode and no GPU**. FBX/OBJ get an analogous header/section scan (§7). Full decode
  (`gltf` crate with `import, names, utils`; `fbxcel`) is EXPENSIVE and happens only on preview.

### 4.2 Enforcement

The cheap contract is enforced two ways: (1) `extract_metadata` never receives anything but the
reader (no GPU handle, no decode context is in scope), and (2) a scan-scale test fixture asserts
`extract_metadata` over the corpus stays within a byte-read and wall-time budget
([15-observability-config-testing-packaging.md](15-observability-config-testing-packaging.md)). A
regression that decodes geometry in the cheap tier shows up as a blown ingest budget.

---

## 5. Per-media attribute structs (cheap-tier outputs)

These are the `MediaAttributes` variants — the low-level realisation of PRODUCT_SPEC §5. They are
filled by `extract_metadata` (cheap tier). Expensive/derived fields (embeddings, BPM, tileability
score, dominant colours, category guesses) are **not** here — they are produced by `extract_features`
and owned by analysis (05); they hang off the asset separately.

```rust
pub struct AudioAttributes {
    pub duration_secs: f64,
    pub sample_rate_hz: u32,
    pub bit_depth: Option<u16>,     // None for lossy/compressed
    pub channels: u16,
    pub codec: String,              // "pcm_s16le", "flac", "vorbis", "mp3", "opus", ...
    pub container: String,          // "wav", "flac", "ogg", "mp4", ...
    // derived (BPM, key, loudness, spectral, class/loop-vs-oneshot) → extract_features → 05
}

pub struct ImageAttributes {
    pub width: u32,
    pub height: u32,
    pub colour_depth_bits: u16,     // per-channel bit depth
    pub has_alpha: bool,
    pub colour_space: ColourSpace,  // Srgb | Linear | Unknown (+ ICC presence flag)
    pub format: String,             // "png", "jpeg", "webp", "tga", "dds", "ktx2", ...
    // derived (phash, dominant colours, tileability score, type classify) → extract_features → 05
}

pub struct ModelAttributes {
    pub vertex_count: u64,          // exact where accessor counts are present, else estimate
    pub triangle_count: u64,
    pub mesh_count: u32,
    pub material_count: u32,
    pub texture_count: u32,
    pub bbox: Aabb,                 // {min, max}; from accessor min/max (cheap) — see 3d notes §4
    pub has_rig: bool,              // any skin / joints referenced
    pub has_animation: bool,        // any animation channel present
    pub has_uvs: bool,              // a TEXCOORD/UV accessor present
    pub format: String,             // "glb", "gltf", "fbx", "obj", "stl", "ply", ...
    // derived (category guess, per-view shape embedding) → extract_features (+ 06 render) → 05
}
```

`Aabb` is the `{min, max}` type from the 3D notes (`../3d-handler-notes.md` §4), reused for camera
framing (06) and later normalisation (05).

---

## 6. Fail-soft behaviour

Fail-soft is an invariant, not a feature (DESIGN_GUIDELINES §2, PRODUCT_SPEC §8). Every handler
method returns `Result<_, HandlerError>`; the orchestrator **records the error against the one
asset and moves on**. Concretely:

- **`detect` finds nothing** → the asset is catalogued as *unknown format* (shared fields only:
  path, size, hash, timestamps). It stays in the library and remains searchable by name/source;
  it simply has no media attributes.
- **`extract_metadata` hits `Truncated`/`Corrupt`** → the asset is catalogued with whatever cheap
  fields were read before the fault (partial `MediaAttributes` are allowed), flagged
  `metadata_error`, and skipped by the expensive tier until re-scanned. **The scan does not abort.**
- **`decode`/`thumbnail`/`extract_features` fail** (only reached at preview/analysis) → that one
  preview or feature is skipped and flagged; the asset keeps its cheap-tier attributes and stays
  usable. A GPU-absent host degrading a 3D render is handled in 06, not here.
- **`Unsupported`** (recognised format, sub-feature we do not read — e.g. an exotic FBX version or a
  codec not in the matrix) → treated like a partial extract: catalogue what we can, flag the rest.

The error taxonomy maps into the `LibraryService` error model (03) so a handler fault surfaces as a
per-asset data-quality flag in the UI/CLI, never as a failed scan.

---

## 7. Format / codec support matrix (v1 vs later)

Concrete first-cut coverage per media type, keyed to the §7 candidate crates (PRODUCT_SPEC) and the
MoGen-confirmed versions (`../3d-handler-notes.md` §5). "v1" = ships in the first media-depth pass;
"Later" = staged behind the same handler once v1 lands. This matrix is the open question below —
it is a plan, not a frozen list.

### 7.1 Audio (`symphonia` decode · `rustfft`/`realfft` features · `cpal`/`rodio` playback)

| Format / codec | Cheap metadata (v1) | Decode + preview (v1) | Notes |
|---|---|---|---|
| WAV (PCM) | ✅ | ✅ | RIFF `fmt `/`data` header scan; the baseline |
| FLAC | ✅ | ✅ | STREAMINFO header for cheap tier |
| Ogg Vorbis | ✅ | ✅ | `OggS` magic; Vorbis identification header |
| MP3 | ✅ | ✅ | ID3 + frame header; `symphonia` |
| Opus (Ogg) | ✅ | ✅ | via `symphonia` |
| AAC / M4A (MP4) | ✅ | Later | ISOBMFF `ftyp` sniff cheap in v1; decode staged |
| AIFF / CAF | Later | Later | container scan straightforward; staged |
| Playback (`cpal`/`rodio`) | — | v1 for the v1-decode set | scrubbable waveform playback (§6.4) |

### 7.2 Image (`image` + `imageproc` · `img_hash` for phash)

| Format | Cheap metadata (v1) | Decode + preview (v1) | Notes |
|---|---|---|---|
| PNG | ✅ | ✅ | `0x89PNG`; IHDR for dims/depth/alpha |
| JPEG | ✅ | ✅ | SOI + SOF header |
| WebP | ✅ | ✅ | RIFF/`WEBP` |
| TGA | ✅ | ✅ | common in game texture packs |
| BMP | ✅ | ✅ | |
| GIF | ✅ | ✅ | first-frame thumbnail |
| DDS | ✅ | Later | header (dims, format, mip count) cheap in v1; block-compressed decode staged |
| KTX2 | ✅ (header) | Later | KTX2 is a convert *target* (§6.5, PNG↔KTX2 in 08); full decode/transcode staged |
| TIFF / EXR / HDR | Later | Later | HDR/linear handling staged; EXR matters for VFX-adjacent packs |
| SVG | — | — | out of scope v1 (vector, not raster asset) |

Colour space / linear handling matters for the tileability test and normal maps (PRODUCT_SPEC §5);
the header-level colour-space flag is cheap-tier, the linear-space analysis is 05.

### 7.3 3D model (`gltf` decode · `fbxcel` for FBX · custom OBJ/STL/PLY readers)

| Format | Cheap metadata (v1) | Full decode + preview (v1) | Notes |
|---|---|---|---|
| glTF (`.gltf`) | ✅ | ✅ | JSON parse for counts; `gltf` crate (`import, names, utils`) for decode |
| GLB (`.glb`) | ✅ | ✅ | **two-tier read** — JSON-chunk scan cheap, BIN decode on preview (`../3d-handler-notes.md` §1) |
| FBX | ✅ | ✅ | `fbxcel` 0.9; header/section scan cheap; the FBX↔glTF convert pair (08) |
| OBJ (+ MTL) | ✅ | ✅ | extension-ordered structural sniff (§3.2); counts from a bounded line scan |
| STL (bin + ascii) | ✅ | ✅ | tri count from header (bin) / bounded scan (ascii) |
| PLY | ✅ | Later | header element counts cheap; decode staged |
| USD / USDZ | Later | Later | high value, heavier dependency; staged |
| glTF Draco-compressed | ✅ (counts) | Later | JSON counts still cheap; Draco geometry decode staged |

### 7.4 Video (discovered `ffprobe`/`ffmpeg` binary — [ADR 0014](../adr/0014-video-decode-backend.md))

| Format | Cheap metadata | Poster frame | Notes |
|---|---|---|---|
| MP4 / M4V (`.mp4`, `.m4v`) | ✅\* | ✅\* | shares the ISO-BMFF container with audio-only `.m4a` — settled by `refine_with_content` against the real track table, not the extension |
| QuickTime (`.mov`) | ✅\* | ✅\* | same container ambiguity as MP4 |
| Matroska / WebM (`.mkv`, `.webm`) | ✅\* | ✅\* | unambiguous by extension |
| AVI (`.avi`), Ogg video (`.ogv`) | ✅\* | ✅\* | coverage is whatever the host's ffmpeg has |

\* **Conditional on the host.** There is no linked decoder and no cargo feature: `dam-media`
discovers `ffprobe`/`ffmpeg` on `PATH` at runtime. With neither, a video is still detected,
catalogued, searchable and playable — it simply has no metadata and shows the typed tile. With
`ffprobe` only, the cheap tier is complete but there is no poster frame. Playback never depends on
any of this: the browser plays the original bytes over the range-capable content route, so **video
has no WASM island and is not a convert target**.

### 7.5 Document (`lopdf` · `zip` + `quick-xml` · `encoding_rs`)

| Format | Cheap metadata | Text extraction | Notes |
|---|---|---|---|
| PDF (`.pdf`) | ✅ | ✅ | page count + info-dict title/author; text via `lopdf`. A scanned-image PDF has no text layer — that is a normal empty result, not an error |
| Markdown (`.md`), plaintext (`.txt`) | ✅ | ✅ | encoding sniffed (BOM → UTF-8 → Windows-1252); `md` title from a leading ATX heading |
| RTF (`.rtf`) | ✅ | ✅ | lexical control-word strip, not a full RTF parse |
| DOCX / ODT | ✅ | ✅ | ZIP + XML: `word/document.xml` / `content.xml` for text, core-properties/`meta.xml` for title+author |
| CSV / JSON | — | — | **deliberately excluded**: structured data, not prose. A later "data" media type if ever wanted |

Documents have **no server-rendered thumbnail** — the tile is the typed glyph and the excerpt is
typeset in the DOM (a server-side PDF raster would need a native PDF renderer + font stack, the
dependency class ADR 0014 declined). Extracted text is the one handler output that does not live on
the asset row: it goes to the `asset_fts` `text` column (schema V10) and to a text embedding space.
Ingest applies an **ignore policy** (`detect_for_ingest`) so documents under dependency/build/VCS
directories never enter the catalog.

---

3D **rendering** (turntable thumbnail, multi-view for shape embeddings, the interactive orbit
viewer, and the GPU-less software-raster fallback) is **not specified here** — the handler hands a
decoded mesh + deterministic framing (bounds-derived `fit_distance`, `../3d-handler-notes.md` §2) to
[06-3d-render.md](06-3d-render.md), which owns wgpu and implements
[ADR 0001](../adr/0001-3d-render-backend.md) / [ADR 0002](../adr/0002-3d-render-crate-boundary.md).
The **shape embedding** those multi-view renders feed is owned by
[05-analysis-similarity-dedup.md](05-analysis-similarity-dedup.md).

---

## 8. What crosses to siblings

- → **[05-analysis-similarity-dedup.md](05-analysis-similarity-dedup.md):** `extract_features`
  returns a `FeatureBundle` (with `extractor_versions` for re-analysis gating). 05 turns it into
  embeddings, perceptual hashes, the ANN index, dedup, tileability scoring, and auto-tag/category.
- → **[06-3d-render.md](06-3d-render.md):** the 3D handler's `thumbnail`/`extract_features` delegate
  all GPU render work (turntable still, multi-view set, software-raster fallback) to `3dam-render`,
  passing a decoded mesh and deterministic framing. 04 never touches wgpu.
- → **[08-convert-pipeline.md](08-convert-pipeline.md):** convert consumes `decode`'s `Decoded`
  output and the concrete `format` string, and writes new files non-destructively. 04 provides the
  read/decode side; 08 owns the encode/optimise side.

---

## Open questions

> **Resolved 2026-07-06 in [ADR 0009 §8](../adr/0009-v1-scope-decisions.md).** The v1 format-decode
> matrix is frozen there (incl. DDS/KTX2, AAC-MP4, PLY/STL; FBX decode-only; USD post-v1), geometry
> counts are marked approximate when accessor counts are absent, and the one-bounded-trailer-seek
> budget stands. Kept below as rationale.

- **Format-coverage matrix for v1 vs later** (carried from PRODUCT_SPEC §10): which loaders and
  codecs actually ship in the first pass. §7 is a first cut; the exact v1 line (e.g. whether MP4/AAC
  audio, DDS/KTX2 image decode, and PLY/USD 3D land in v1 or stage later) is not frozen and depends
  on crate maturity and spikes. The cheap-tier sniff for a format is cheap to add ahead of its
  decoder, so a format can appear in the catalog (metadata-only) before its preview/convert support.
- **Cheap-tier trailer reads.** A few formats carry needed data in a trailer (or require a second
  seek). The read budget (§4.2) assumes one bounded trailer seek is acceptable at ingest scale;
  formats that would need more than that may have to defer some "cheap" fields to the expensive tier.
- **Estimate vs exact geometry counts.** Where accessor `count` fields are absent (some OBJ/FBX
  paths), `ModelAttributes.vertex_count`/`triangle_count` are estimates; whether the cheap tier does
  a bounded second pass for exactness, or marks the value approximate, is unsettled.
