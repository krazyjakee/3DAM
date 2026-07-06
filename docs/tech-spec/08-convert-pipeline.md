# 08 — Convert / Compress / Optimise Pipeline

Status: **Draft v0.1** · Scope: the convert-job model, per-media conversion design, batching, dry-run/preview, and the non-destructive output policy.

This file owns *how a conversion actually happens* once a job has been submitted: the job data
model, its per-item lifecycle, the per-media conversion parameters and the crates that carry
them out, batching semantics, the dry-run/preview contract, and the output-path/collision
policy that keeps conversion non-destructive (DESIGN_GUIDELINES §1.3, §6; PRODUCT_SPEC §6.5).

It sits **below** the API and **above** the media handlers:

- Jobs are **submitted and tracked** through the `LibraryService` job API — see
  [03-library-service-and-api.md](03-library-service-and-api.md). This file defines the
  convert-job *payload* and *result* types that 03 transports and 13 fills from CLI flags; it
  does **not** redefine the job-submission trait, progress-stream, or error model.
- Conversion **reuses `MediaHandler` decode and format detection** from
  [04-media-handlers.md](04-media-handlers.md). This file does not re-specify decoding or the
  format matrix; it consumes them and adds the *encode* side.
- The `convert` **CLI verb surface** (flags, `--json`, exit codes) is
  [13-cli.md](13-cli.md)'s. This file gives 13 the parameter types to bind flags onto.
- The **worker pool, cancellation, and fail-soft mechanics** are
  [14-concurrency-performance-reliability.md](14-concurrency-performance-reliability.md)'s. This
  file describes *what* runs per item and *what state it moves through*; 14 owns *how* items are
  scheduled onto rayon/tokio and how cancellation propagates.

Product rationale (why non-destructive, why batch preview) is in PRODUCT_SPEC §6.5 and
DESIGN_GUIDELINES §3.4 — cited, not restated.

---

## 1. The convert-job model

A convert job is a **plan** — an input asset set, a single target spec, and an output-location
policy — that expands into one **item** per input asset. Items are processed independently and
**fail-soft**: one unreadable or unencodable asset fails its own item and the batch continues
(DESIGN_GUIDELINES §2, §6; PRODUCT_SPEC §8).

These are the load-bearing type names. Files 03 (transport DTOs), 13 (CLI binding), and 11 (MCP
`convert` tool) reference them; keep them stable.

```rust
/// The submitted plan. One job → many items (one per input asset).
pub struct ConvertJob {
    pub id: JobId,                       // allocated by LibraryService (03)
    pub inputs: Vec<AssetId>,            // resolved input asset set (§4)
    pub target: ConvertTarget,           // one target spec for the whole batch
    pub output: OutputPolicy,            // where results land + collision handling (§5)
    pub mode: RunMode,                   // DryRun | Preview | Commit (§4)
    pub options: JobOptions,             // overwrite/collision/parallelism knobs
}

pub enum RunMode {
    /// Plan only: resolve outputs, estimate sizes, detect collisions. Writes nothing. (§4.2)
    DryRun,
    /// Encode a single item to a temp/preview buffer for inspection, discard it. (§4.3)
    Preview { asset: AssetId },
    /// Real run: write outputs under the OutputPolicy. (§4.4)
    Commit,
}

pub struct JobOptions {
    /// Cap on concurrent items; None → pool default (delegated to 14).
    pub max_parallel: Option<usize>,
    /// If false (default), any per-item output-path collision fails that item (§5.3).
    pub allow_overwrite: bool,
    /// Continue past failed items (default true = fail-soft). false = abort batch on first fail.
    pub fail_fast: bool,
}
```

`ConvertTarget` is the media-typed encode spec (§3). It is a tagged union so one job carries
exactly one target; a batch that mixes media types is rejected at submission unless the target
is `Auto` (§4.1).

```rust
pub enum ConvertTarget {
    Audio(AudioTarget),
    Image(ImageTarget),
    Model(ModelTarget),
    /// Per-item target chosen from the item's media type using a named profile.
    /// Lets one `convert --profile web-optimise` span audio+image+3D. (§3.5)
    Auto { profile: ConvertProfile },
}
```

### 1.1 Item lifecycle / state machine

Each input asset becomes a `ConvertItem` with an independent state. States advance
monotonically except `Cancelled`, which any non-terminal state can enter (cancellation
plumbing is 14's).

```
                 ┌─────────┐
                 │ Queued  │
                 └────┬────┘
        (worker picks up; decode via MediaHandler)
                      ▼
                 ┌─────────┐        decode/encode error, unsupported target
                 │ Running │ ─────────────────────────────┐
                 └────┬────┘                               ▼
     write to temp, verify, atomic rename            ┌──────────┐
                      ▼                               │  Failed  │ (terminal, per-item)
                 ┌─────────┐                          └──────────┘
                 │  Done   │ (terminal)                     ▲
                 └─────────┘                                │
                      ▲                                     │
              collision, no overwrite ─────────────────────┘  (→ Failed{Collision})

   any non-terminal ──cancel──► Cancelled (terminal)
```

```rust
pub struct ConvertItem {
    pub input: AssetId,
    pub state: ItemState,
    pub planned_output: Option<PathBuf>, // set once resolved (dry-run and commit both fill this)
    pub estimate: Option<OutputEstimate>,// filled during planning (§4.2)
    pub result: Option<ItemResult>,      // filled on Done
}

pub enum ItemState {
    Queued,
    Running { phase: Phase },  // Decoding | Encoding | Writing | Verifying
    Done,
    Failed(ConvertError),      // maps into 03's error model; does NOT sink the batch
    Cancelled,
}

pub struct ItemResult {
    pub output_path: PathBuf,
    pub output_bytes: u64,
    pub input_bytes: u64,
    pub ratio: f32,            // output_bytes / input_bytes
    pub output_format: Format, // concrete written format (04's Format)
    pub content_hash: Hash,    // hash of the written file (02) — feeds derivative/export links (§6)
}

pub enum ConvertError {
    Decode(MediaHandlerError),    // reused from 04
    UnsupportedTarget { from: Format, to: Format },
    Encode(String),
    Collision { path: PathBuf },  // output already exists and overwrite not allowed
    Io(String),
    Cancelled,
}
```

`ConvertError` variants are surfaced per item through 03's error model; the **job as a whole**
succeeds as long as it ran, even if some items are `Failed` (fail-soft). A job's terminal
summary reports counts.

```rust
pub struct JobSummary {
    pub job: JobId,
    pub done: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub total_input_bytes: u64,
    pub total_output_bytes: u64,
    pub items: Vec<ConvertItem>,   // full per-item detail, incl. errors
}
```

### 1.2 Progress reporting

Convert emits the **same incremental progress events** 03/14 define for any long job — this
file only names the payload. Progress is per-item so the UI/CLI can render a live list
(DESIGN_GUIDELINES §1.1, §3.4), never a single terminal dump.

```rust
pub enum ConvertProgress {
    Planned  { total: usize, estimate: OutputEstimate },   // after dry-run planning of the batch
    Item     { input: AssetId, state: ItemState, done: usize, total: usize },
    Finished (JobSummary),
}
```

The event stream is delivered by the `LibraryService` job-progress channel
([03](03-library-service-and-api.md)); the worker pool that drives it is
[14](14-concurrency-performance-reliability.md)'s. This file does not own either.

---

## 2. How conversion reuses `MediaHandler` (04)

The **decode** side is entirely 04's. A convert item:

1. Loads the input asset's `Format` (already known from ingest, or re-detected via
   `MediaHandler::detect` — [04](04-media-handlers.md)).
2. Calls the handler's decode entry point to obtain the in-memory media
   (`DecodedAudio` / `DecodedImage` / `LoadedModel` — 04's decode outputs), **reusing** the
   same decoders ingest/preview use. Convert adds no new decoders.
3. Applies the media-typed transform + **encode** defined here (§3).
4. Writes the encoded bytes under the `OutputPolicy` (§5).

The `MediaHandler` trait (04) is therefore extended on its *output* side with an encode
capability, kept separate from the cost-tiered read methods so a handler can decode-only or
support a subset of encode targets:

```rust
/// Encode half of a media handler. Lives in 3dam-core alongside MediaHandler (04);
/// documented here because convert is its only caller.
pub trait MediaEncoder {
    type Decoded;               // DecodedAudio | DecodedImage | LoadedModel (04)
    type Target;                // AudioTarget | ImageTarget | ModelTarget (§3)

    /// Which targets this handler can produce from a given source format.
    fn supported_targets(&self, from: Format) -> Vec<Format>;

    /// Cheap planning: estimate output size/params WITHOUT full encode (§4.2).
    fn estimate(&self, decoded_meta: &DecodedMeta, target: &Self::Target) -> OutputEstimate;

    /// Real encode into a byte sink (a temp file writer, §5.2).
    fn encode(&self, decoded: &Self::Decoded, target: &Self::Target, out: &mut dyn Write)
        -> Result<EncodeReport, ConvertError>;
}
```

`estimate` runs off metadata/header math (bitrate × duration, dimensions × format cost, tri
count × vertex stride) so a full-batch dry-run does not decode-and-encode everything — it may
decode headers only. `encode` does the real work and is what 14 schedules onto a worker.

---

## 3. Per-media conversion

One target type per media, each a plain params struct that 13 binds CLI flags onto and 03
transports verbatim. Crates follow PRODUCT_SPEC §7; concrete matrices (which source formats
decode) are 04's.

| Media | Target type | Params (this file) | Encode via | Notes |
|-------|-------------|--------------------|-----------|-------|
| Audio | `AudioTarget` | codec, sample rate, bit depth, channels, quality/bitrate, loudness-normalise | `symphonia` decode (04) → format encoders (WAV/FLAC native; Ogg/Vorbis/Opus, MP3/AAC via bindings) | Resample + dither on bit-depth reduction; optional loudness normalise reuses the analysis loudness (05). |
| Image | `ImageTarget` | format, dimensions/resize, quality, compression, mip generation, colour space, premultiply-alpha | `image`/`imageproc` decode (04) → PNG/JPEG/WebP encoders; **KTX2/Basis** (UASTC/ETC1S) via `ktx2` + `basis-universal`/`intel_tex` for GPU textures | PNG↔KTX2 is the headline path (PRODUCT_SPEC §6.5); mip + block-compression are texture-pipeline params. |
| 3D | `ModelTarget` | container format, mesh optimisation flags, quantisation, Draco/meshopt compression, texture handling, scene flattening | `gltf`/loaders decode (04) → glTF/GLB writer; **`meshopt`** for optimise+compress (PRODUCT_SPEC §7) | FBX↔glTF is the headline path; mesh optimisation is a distinct stage from container transcode (§3.3). |

### 3.1 Audio

```rust
pub struct AudioTarget {
    pub codec: AudioCodec,               // Wav | Flac | Vorbis | Opus | Mp3 | Aac
    pub sample_rate: Option<u32>,        // None → keep source
    pub bit_depth: Option<BitDepth>,     // Pcm16 | Pcm24 | Pcm32 | F32; None → keep
    pub channels: Option<ChannelPolicy>, // Keep | Mono(downmix) | Stereo
    pub quality: AudioQuality,           // Lossless | Vbr(u8) | Cbr(kbps)
    pub loudness: Option<LoudnessTarget>,// None | Lufs(f32) — reuses 05's loudness feature
}
```

Sample-rate change goes through a resampler (`rubato`); bit-depth reduction applies dither.
`quality` is ignored for lossless codecs and validated against the codec at submission (e.g.
`Cbr` on `Flac` is rejected — see §4.1).

### 3.2 Image

```rust
pub struct ImageTarget {
    pub format: ImageFormat,             // Png | Jpeg | Webp | Ktx2 | Basis | Tga | Exr
    pub resize: Option<Resize>,          // Fit/Fill/Exact WxH; None → keep
    pub quality: Option<u8>,             // lossy formats only (0..=100)
    pub compression: TextureCompression, // None | Bc7 | Bc5 | Etc1s | Uastc (KTX2/Basis)
    pub mips: MipPolicy,                 // None | Generate | Keep
    pub color_space: Option<ColorSpace>, // Srgb | Linear — respect normal-map linearity (05)
    pub premultiply_alpha: bool,
}
```

`compression` is only valid for `Ktx2`/`Basis`; `quality`/`compression` are mutually
constrained by `format` and checked at submission.

### 3.3 3D — two distinct stages

3D conversion separates **container transcode** (FBX→glTF) from **mesh optimisation/compression**
(`meshopt`), because they are independently useful: you can optimise a mesh in-place (glTF→glTF)
or transcode without touching geometry.

```rust
pub struct ModelTarget {
    pub container: ModelFormat,          // Gltf | Glb | Obj  (FBX is decode-in via 04, not an encode target in v1)
    pub optimise: MeshOptimise,          // meshopt vertex-cache/overdraw/fetch passes
    pub quantise: Option<Quantisation>,  // position/normal/uv bit widths
    pub compression: MeshCompression,    // None | Meshopt | Draco
    pub textures: TexturePolicy,         // Embed | ExternalRefs | Convert(ImageTarget)  (§3.4)
    pub flatten: bool,                   // collapse node hierarchy / bake transforms
}

pub struct MeshOptimise {
    pub vertex_cache: bool,   // meshopt_optimizeVertexCache
    pub overdraw: bool,       // meshopt_optimizeOverdraw
    pub vertex_fetch: bool,   // meshopt_optimizeVertexFetch
    pub simplify: Option<f32>,// target ratio 0..1 via meshopt_simplify; None = no LOD reduction
    pub weld: bool,           // dedup vertices before optimising
}
```

`FBX` is a **decode-only** source in v1 (via 04's loader); the encode targets are the open glTF
family plus OBJ, matching PRODUCT_SPEC §6.5's FBX↔glTF framing where the "↔" back to FBX is a
documented later target, not v1 scope (§7 Open questions).

### 3.4 Textures inside a 3D convert

`TexturePolicy::Convert(ImageTarget)` runs the **image** pipeline (§3.2) over each referenced
texture as a sub-step of the model job — e.g. `convert scene.fbx --to glb --textures ktx2` bakes
PNG textures to KTX2 and rewrites the glTF references. Each texture conversion is its own encode
but rolls up into the parent model item's `ItemResult` (no separate item id).

### 3.5 Auto profiles

`ConvertTarget::Auto { profile }` lets one job span media types by selecting a per-item target
from a named `ConvertProfile` (e.g. `web-optimise`, `engine-import`). Profiles are named presets
resolving to the three concrete targets above; the profile registry is small and lives in
`3dam-core`. This is what makes `3dam convert <mixed-selection> --profile web-optimise` (13)
well-defined.

---

## 4. Batching, dry-run, and preview

One job, many assets (PRODUCT_SPEC §6.5). The three `RunMode`s share planning and differ only
in what they write.

### 4.1 Input resolution & validation (all modes)

Submission resolves `inputs` to concrete assets (an explicit id list, or a saved
search/smart-folder query handed off to 03's query API) and validates the batch:

- **Media-type match.** A non-`Auto` target requires every input to be that media type;
  mismatches are rejected up front with a clear error (not per-item failures).
- **Param validity.** Codec/format/quality constraints (§3.1–§3.3) are checked once.
- **Support check.** `MediaEncoder::supported_targets(from)` (§2) is consulted per distinct
  source format; unsupported (from → to) pairs become planned `Failed{UnsupportedTarget}` items
  rather than aborting submission (still fail-soft).

### 4.2 Dry-run — report what WOULD be written

`RunMode::DryRun` runs the **planning pass only**: resolve each item's output path (§5), detect
collisions, and compute an `OutputEstimate` from `MediaEncoder::estimate` (metadata math, no
full encode). It writes nothing (DESIGN_GUIDELINES §5, PRODUCT_SPEC §6.5). This is what
`convert --dry-run` (13) returns and what the GUI bulk-op preview (DESIGN_GUIDELINES §3.4) shows
before commit.

```rust
pub struct OutputEstimate {
    pub est_output_bytes: u64,      // per item; summed for the batch
    pub est_ratio: f32,            // est_output_bytes / input_bytes
    pub confidence: EstConfidence, // Exact (lossless/known) | Approx (heuristic)
}

/// The dry-run result shape (03 transports it; 13 renders it as a table / --json).
pub struct DryRunReport {
    pub job: JobId,
    pub target: ConvertTarget,
    pub output_root: PathBuf,
    pub items: Vec<DryRunItem>,
    pub total_input_bytes: u64,
    pub total_est_output_bytes: u64,
    pub collisions: usize,          // items whose planned path already exists
    pub unsupported: usize,         // items that cannot be converted
}

pub struct DryRunItem {
    pub input: AssetId,
    pub input_path: PathBuf,
    pub planned_output: PathBuf,    // the exact path that WOULD be written
    pub estimate: OutputEstimate,
    pub disposition: Disposition,   // Write | Collision | Unsupported
}
```

A dry-run never allocates real output files; `planned_output` is the resolved-but-unwritten
path so the user sees the full write plan. `--json` (13) emits `DryRunReport` verbatim.

### 4.3 Preview — effect before committing

`RunMode::Preview { asset }` performs a **real single-item encode to an in-memory/temp buffer**
and returns it for inspection (a converted thumbnail/waveform, size, ratio) **without writing to
the output root** — the buffer is discarded after inspection. This backs the "preview its
effect" requirement (DESIGN_GUIDELINES §3.4) and lets the inspector show, say, the KTX2/BC7
result on one texture before the user commits the batch.

```rust
pub struct PreviewResult {
    pub input: AssetId,
    pub output_format: Format,
    pub output_bytes: u64,
    pub ratio: f32,
    pub preview: PreviewBlob,  // decoded-back thumbnail / waveform of the converted output
}
```

Preview is deliberately one item: it is an interactive spot-check, not a batch operation.

### 4.4 Commit

`RunMode::Commit` runs the full per-item lifecycle (§1.1) across the batch on the worker pool
(14), writing under the `OutputPolicy` (§5) and streaming `ConvertProgress` (§1.2). Undo is the
non-destructive guarantee itself: since sources are untouched and outputs go to a chosen
location, "undo" is deleting the produced outputs (DESIGN_GUIDELINES §3.4 "undoable where
physically possible") — the `JobSummary` lists every written path so the caller can offer it.

---

## 5. Non-destructive output policy

Outputs go to a **user-chosen location** and **never overwrite sources silently** (PRODUCT_SPEC
§6.5, DESIGN_GUIDELINES §1.3, §6). This is enforced structurally: the encode target is *always*
a fresh path under `OutputPolicy`, never the input path.

```rust
pub struct OutputPolicy {
    pub root: PathBuf,               // user-chosen destination directory (required)
    pub layout: OutputLayout,        // how per-item paths are derived under root
    pub naming: NamingRule,          // filename stem + extension derivation
    pub on_collision: CollisionRule, // Fail | Suffix | Skip | Overwrite (§5.3)
}

pub enum OutputLayout {
    Flat,                            // all outputs directly under root
    MirrorSource,                    // recreate each input's source-relative dir under root
    BySource,                        // group by source id/name
}

pub enum NamingRule {
    /// Keep stem, swap extension for the target format: foo.png → foo.ktx2
    SwapExtension,
    /// Append a suffix before the new extension: foo.png → foo.optimised.ktx2
    Suffix(String),
    /// Explicit template with {stem}, {ext}, {format}, {id} placeholders.
    Template(String),
}

pub enum CollisionRule {
    Fail,        // default: planned path exists → item Failed{Collision} (§1.1)
    Suffix,      // foo.ktx2 → foo-1.ktx2, foo-2.ktx2 …
    Skip,        // leave existing file, mark item Done{skipped}
    Overwrite,   // only if JobOptions.allow_overwrite AND path is NOT a source (§5.1)
}
```

### 5.1 The source-safety invariant

Regardless of `CollisionRule`, a resolved output path that equals or lies inside a **registered
source tree** (02's source records) is **always rejected** — `Overwrite` cannot target a source
file. This is the hard non-destructive line: 3DAM will not, under any flag combination, write
over a catalogued original. In-place conversion is expressed as "output to a different root",
never as overwriting the source.

### 5.2 Atomic writes

Every item writes to a temp file in the destination filesystem, `fsync`s, verifies the encode
(and hashes it → `ItemResult.content_hash`, 02), then **atomically renames** into
`planned_output`. A crash or cancel mid-encode leaves the temp file (garbage-collected on next
run), never a half-written output at the real path. This makes commit safe to retry.

### 5.3 Collision resolution order

At plan time each item's path is resolved, then collisions are settled by `on_collision`:
`Fail` (default) surfaces them in the dry-run `collisions` count and fails the items at commit;
`Suffix` deterministically disambiguates; `Skip` no-ops existing outputs; `Overwrite` replaces
**only** non-source files and **only** when `allow_overwrite` is set (§5.1 still bars sources).
Two items in the *same batch* resolving to the same path is always a `Suffix`/`Fail` case, never
a silent last-writer-wins.

---

## 6. Manifest & export hooks

Convert outputs are **new files**, not new catalogued assets by default (they live at the
user's chosen root, which may be outside any source). Two integration points, both owned
elsewhere and only *linked* from convert:

- **`content_hash` on every `ItemResult`** (§1.1) lets a caller relate an output back to its
  input asset and target — the basis for a convert **manifest** (input id → output path, format,
  ratio, hash). The manifest/export format and the `export` verb are 03's data-model + export
  surface ([03-library-service-and-api.md](03-library-service-and-api.md); PRODUCT_SPEC §6.6);
  convert supplies the rows via `JobSummary`, it does not define the export file format.
- **Re-ingesting outputs is opt-in.** If the output root is (or becomes) a registered source,
  the normal scan/ingest path (03/04) catalogues the results like any other files — convert does
  not special-case them. There is no hidden coupling between producing an output and cataloguing
  it.

Derivative/blob caching (thumbnails of converted previews) is 02's cache; convert previews
(§4.3) are transient and are not persisted as derivatives.

---

## 7. Cross-file contract (what neighbours must honour)

- **03** transports `ConvertJob`, `ConvertTarget`, `RunMode`, `OutputPolicy`, `DryRunReport`,
  `PreviewResult`, `ConvertProgress`, and `JobSummary` as DTOs, and allocates `JobId`; it owns
  submission/tracking, the progress channel, and the error model these map into.
- **04** provides decode + `Format` detection and hosts `MediaEncoder`; convert adds no decoders.
- **13** binds `convert` CLI flags onto `AudioTarget`/`ImageTarget`/`ModelTarget`/`ConvertProfile`
  and `OutputPolicy`, and renders `DryRunReport`/`JobSummary` as human/`--json` output;
  `--dry-run` ⇒ `RunMode::DryRun`, `--yes` gates `Overwrite`.
- **14** schedules `ConvertItem` encodes on the worker pool and owns cancellation/fail-soft
  mechanics; this file only defines item states and the fail-soft policy those mechanics honour.
- **11** (MCP `convert` tool) reuses the same types; write-gating/dry-run defaults are 11's.

---

## Open questions

> **Resolved 2026-07-06 in [ADR 0009 §7/§8](../adr/0009-v1-scope-decisions.md).** v1 encode targets
> = glTF family + OBJ (FBX/USD encode post-v1); the convert manifest is a first-class persisted
> record; commits are idempotent by `content_hash`; profiles are built-in-only for v1; estimation
> stays `Approx` (real-encode calibration post-v1). Kept below as rationale.

- **FBX (and other proprietary) write-back.** v1 encodes to the open glTF family + OBJ only
  (§3.3); whether FBX/USD *encode* targets ship later (licensing, loader-writer availability)
  is deferred (PRODUCT_SPEC §6.5, §10 format matrix).
- **Estimate accuracy for lossy/GPU-compressed targets.** `OutputEstimate` for KTX2/Basis, Draco,
  and VBR audio is heuristic (`EstConfidence::Approx`); how far to invest in tighter models vs a
  fast-real-encode-sample of one item to calibrate the batch is open (§4.2).
- **Convert-manifest as a first-class record.** Whether a committed job's manifest persists in
  the library (queryable "what did I export where") or stays a transient `JobSummary` the caller
  writes out — depends on 02's data model and 03's export design (§6).
- **Profile registry surface.** Whether `ConvertProfile` presets (§3.5) are built-in only or
  user-definable (and where such definitions live) — ties to config precedence (15).
- **Cross-media `Auto` estimation cost.** A batch dry-run over mixed media may need to touch
  three decoders; whether that stays header-only at scale or needs sampling is a perf question
  for 14.
- **Resume/idempotency of a commit.** Atomic-rename makes retry safe (§5.2), but whether a
  re-submitted job skips already-produced identical outputs (by `content_hash`) or re-encodes is
  unspecified.

---

See also: [03-library-service-and-api.md](03-library-service-and-api.md) ·
[04-media-handlers.md](04-media-handlers.md) · [13-cli.md](13-cli.md) ·
[14-concurrency-performance-reliability.md](14-concurrency-performance-reliability.md) ·
[00-overview.md](00-overview.md) · [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §6.5, §6.6, §7 ·
[DESIGN_GUIDELINES.md](../DESIGN_GUIDELINES.md) §1.3, §3.4, §5, §6
