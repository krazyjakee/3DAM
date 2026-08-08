# 04 — Media handlers

Status: **Draft v0.1** · Scope: the `MediaHandler` plugin layer — detection, decode, cost-tiered metadata/thumbnail/feature extraction, and the per-media format matrix.

This file specifies the **media-handler seam**: the trait every media type (audio, image, 3D)
implements, how a handler is selected from the logical-path signal available during a source walk
and refined from content after fetch, and the **cost-tiered contract** that keeps ingest cheap. It is the low-level design
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
calls the handler registry owned by `3dam-media`; scanning, analysis, and preview orchestration call
*into* handlers and persist what comes back.

```
   scan/analysis/preview orchestration  (3dam-core — owns concurrency, persistence, fail-soft)
                    │
                    ▼   selects by logical path; refines ambiguous containers after fetch
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

The implementation lives in `dam-media` and is intentionally path-based today. Scan discovery has
a source-relative logical path before it has local bytes (a remote fetch receives a random temporary
name), while the existing format modules operate on a local `Path` once bytes are fetched. The
trait therefore captures the real dispatch seam without inventing a reader/decoded abstraction the
code cannot yet honour.

### 2.1 Shared types

```rust
pub struct FormatId {
    pub media: MediaType,
    pub format: &'static str,
    pub confidence: Confidence,
}

pub enum Confidence { Magic, Container, ExtensionOnly }

pub struct ThumbPng {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}
```

`Detected` remains the persisted/API compatibility shape: an owned concrete-format string plus
`MediaType`. `FormatId` is the registry-selection result and records how the choice was made.

### 2.2 The trait

```rust
pub trait MediaHandler: Send + Sync {
    fn media_type(&self) -> MediaType;

    // CHEAP: logical-path detection, then header/container metadata.
    fn detect(&self, path: &Path) -> Option<FormatId>;
    fn extract_metadata(&self, path: &Path, format: &str) -> MediaAttributes;

    // EXPENSIVE: full decode/render work, only on demand.
    fn render_thumbnail(
        &self,
        path: &Path,
        format: &str,
        max_edge: u32,
    ) -> Result<ThumbPng, HandlerError>;

    // EXPENSIVE document-only surface; other handlers return None.
    fn extract_text(&self, path: &Path, format: &str) -> Option<String>;
}
```

There are five stateless built-in implementations: audio, image (including DDS/KTX texture
routing), model, video, and document. They are deliberately thin wrappers over the established
module functions, so installing the seam changes dispatch rather than media behaviour. Unsupported
thumbnail/text methods use trait defaults.

**Why the split.** `detect` + `extract_metadata` are the **CHEAP tier**;
`render_thumbnail` + `extract_text` are the **EXPENSIVE tier**. Decode, convert, and feature
entry points retain their existing typed APIs until they have a real common decoded representation;
they remain expensive by contract and can be added to the trait without changing registry
selection.

---

## 3. Format detection & handler selection

Selection has two stages because source walking and byte access happen at different times.

### 3.1 The registry

`dam-media::HandlerRegistry` owns the deterministic built-in handler list. `routes` through the
registry are used by the public `detect`, `detect_for_ingest`, `extract_metadata`,
`render_thumbnail`, and `extract_text` entry points. `select_handler(path)` returns
`(FormatId, &dyn MediaHandler)`; callers that already hold a persisted `Detected` select the
same handler by `MediaType`.

Registration is currently compile-time and stateless. Optional capabilities remain feature-gated
inside their handler (for example model conversion/Assimp), so a disabled expensive capability
answers `Unsupported` without removing cheap detection coverage.

### 3.2 The selection flow

```text
select_handler(logical_path):
  1. each built-in handler sees the lowercase extension hint in registry order
  2. the first exact family/format mapping returns FormatId { ExtensionOnly }
  3. no claim returns None; the caller catalogs/skips it exactly as before

after fetch, for an ambiguous ISO-BMFF extension:
  4. refine_with_content runs the bounded ffprobe stream check
  5. an audio-only mp4/mov/m4v corrects Video -> Audio; otherwise the provisional result stands
```

This preserves two load-bearing behaviours:

- `detect` is pure, cheap, and filesystem-independent because scans call it with a logical remote
  path before bytes exist locally.
- The document ingest ignore policy remains outside handler selection. `detect_for_ingest` first
  obtains the registry result, then filters only documents under dependency/build/VCS directories,
  except licence evidence at any depth. Non-document media is never filtered.

Aliases are normalised by the owning handler (`jpeg -> jpg`, `tif -> tiff`, `aif -> aiff`,
`oga -> ogg`, `markdown -> md`). Extension-less licence evidence is claimed by the document
handler as plaintext.

### 3.3 Selection output

`select_handler` returns `(FormatId, &dyn MediaHandler)`. Current walk-time selections carry
`ExtensionOnly`; `Magic` and `Container` are represented so a future reader-based sniff can
strengthen confidence without changing the persisted `Detected` compatibility type. The later
ISO-BMFF refinement is content-based but currently returns only a corrected `Detected`, matching
the scan API that consumes it.

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

Concrete shipped coverage per media type. "Later" is explicitly outside the v1 line frozen by
[ADR 0009](../adr/0009-v1-scope-decisions.md); a recognised but unavailable decoder remains a
fail-soft per-item result rather than aborting a scan or batch.

### 7.1 Audio (`symphonia` decode · `rustfft`/`realfft` features · `cpal`/`rodio` playback)

| Format / codec | Cheap metadata (v1) | Decode + preview (v1) | Notes |
|---|---|---|---|
| WAV (PCM) | ✅ | ✅ | RIFF `fmt `/`data` header scan; the baseline |
| FLAC | ✅ | ✅ | STREAMINFO header for cheap tier |
| Ogg Vorbis | ✅ | ✅ | `OggS` magic; Vorbis identification header |
| MP3 | ✅ | ✅ | ID3 + frame header; `symphonia` |
| Opus (Ogg) | ✅ | ✅ | via `symphonia` |
| AAC / M4A (MP4) | ✅ | ✅ | `symphonia` ISOBMFF + AAC decode; waveform preview and audio→WAV use the same PCM path |
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
| DDS | ✅ | ✅ | header (dims, format, mip count) cheap; uncompressed + BC1–BC7 + ASTC decode (issue #49) |
| KTX2 | ✅ | ✅ / partial | uncompressed RGBA/BGRA, **BC1–BC7, ETC2/EAC, and 2D LDR ASTC** decode; supercompressed Basis/ETC1S/UASTC, Zstd/ZLIB, HDR, and 3D payloads remain metadata-only and fail softly at preview/convert |
| KTX (v1) | ✅ (recognised) | — | different container/magic; reported as undecoded rather than as a corrupt KTX2 |
| TIFF | ✅ | ✅ | `image` decode and raster preview/convert |
| EXR / HDR | Later | Later | HDR/linear handling staged; EXR matters for VFX-adjacent packs |
| SVG | — | — | out of scope v1 (vector, not raster asset) |

Colour space / linear handling matters for the tileability test and normal maps (PRODUCT_SPEC §5);
the header-level colour-space flag is cheap-tier, the linear-space analysis is 05.

Two notes on the texture containers (issue #49), because both are easy to get subtly wrong:

- **Not `image`'s own `dds` feature.** It decodes DXT1/DXT3/DXT5 and returns `Unsupported` for
  everything else — which excludes **BC5** (how normal maps are stored) and **BC7** (the modern
  albedo default), i.e. most of a current texture set. Enabling it would have read as support while
  failing on the common cases.
- **Colour space is reported only when the file states it.** A KTX2 spells it into the `VkFormat`
  (`..._SRGB`), and a DX10 DDS carries it in the DXGI format — those are answered. A **DX9** DDS
  header has no such field, so the answer is `None` rather than a guess: defaulting it to linear
  would mislabel every legacy albedo, and defaulting to sRGB would mislabel every normal map.

### 7.3 3D model (bounded structural metadata · Assimp full decode/render)

| Format | Cheap metadata (v1) | Full decode + preview (v1) | Notes |
|---|---|---|---|
| glTF (`.gltf`) | ✅ | ✅ | JSON parse for counts; `gltf` crate (`import, names, utils`) for decode |
| GLB (`.glb`) | ✅ | ✅ | **two-tier read** — JSON-chunk scan cheap, BIN decode on preview (`../3d-handler-notes.md` §1) |
| FBX | ✅ | ✅ | bounded header/section scan cheap; Assimp full decode and textured preview; decode-only in v1 |
| OBJ (+ MTL) | ✅ | ✅ | extension-ordered structural sniff (§3.2); counts from a bounded line scan |
| STL (bin + ascii) | ✅ | ✅ | tri count from header (bin) / bounded scan (ascii) |
| PLY | ✅ | ✅ | header element counts cheap; Assimp full decode and preview |
| USD / USDZ | Later | Later | high value, heavier dependency; staged |
| glTF Draco-compressed | ✅ (counts) | Later | JSON counts still cheap; Draco geometry decode staged |

### 7.4 Video (discovered `ffprobe`/`ffmpeg` binary — [ADR 0015](../adr/0015-video-decode-backend.md))

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
dependency class ADR 0015 declined). Extracted text is the one handler output that does not live on
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
> budget stands.

> **Handler seam resolved 2026-08-08 (issue #170).** `MediaHandler`, the five built-in wrappers,
> `HandlerRegistry`, and `select_handler` now exist. All public detect/metadata/thumbnail/text
> dispatch passes through that registry, with document ingest filtering retained as a caller policy.

- **Reader-based confidence upgrades.** Walk-time detection must remain filesystem-independent for
  remote sources, so current registry choices are `ExtensionOnly` and ambiguous ISO-BMFF content
  is refined after fetch. A future source-neutral bounded reader can add `Magic`/`Container`
  confidence without changing the handler or registry shapes.
- **Common decoded representation.** Convert and feature APIs remain strongly typed because the
  code has no honest shared `Decoded` value yet. Add trait methods only when that representation
  exists; an enum added merely to box today's unrelated return types would not create a plugin seam.
- **Format-coverage rationale.** Cheap detection may land ahead of an expensive decoder, so a
  recognised format can appear in the catalog while preview/convert answers `Unsupported`. The
  frozen v1 boundary is the table above and ADR 0009.
