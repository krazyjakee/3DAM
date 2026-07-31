# ADR 0015 — Video decode backend: a discovered `ffmpeg`/`ffprobe` binary, not a linked library

Status: **Accepted** · Date: 2026-07-30 · Deciders: 3DAM core
Supersedes: — · Related: [ADR 0009](0009-v1-scope-decisions.md) (v1 media scope), [ADR 0011](0011-assimp-import-backend.md) (the other native-decode dependency), [tech-spec 04](../tech-spec/04-media-handlers.md), issue [#79](https://github.com/krazyjakee/3DAM/issues/79)

## Context

Adding `MediaType::Video` (issue #79) needs two things from a decoder: the **cheap tier**
(duration, width/height, fps, codec, container, bitrate, has-audio) and **one poster frame** at
roughly 10% of duration for the thumbnail. Playback is *not* our problem — the browser plays the
bytes natively through `<video>`, so we never decode for preview, only for a static tile.

That is a deliberately small ask, and it is worth being precise about it, because the obvious
answer — link ffmpeg — is priced for a much larger one.

The landscape:

- **Pure-Rust demuxers** (`symphonia`'s isomp4, `matroska`, `mp4parse`) read container boxes fine.
  They give duration, track table, and codec identifiers for MP4/MKV, less for AVI, and they do
  **not** decode a frame. Poster frames would be impossible, and the epic calls the poster frame
  "the one media type where a static server-side thumbnail is straightforwardly correct."
- **`ffmpeg-next`** (bindings to libav*) does everything, in-process, with the best fidelity. It
  also links a large native C library into our build: a `pkg-config`/vcpkg hunt on three OSes, a
  meaningful cross-compilation burden, and a licence posture that has to be actively managed
  (LGPL if you are careful about which components are enabled, GPL the moment `--enable-gpl`
  components come along for the ride — and distro builds vary). Phase 7 is
  packaging/distribution for Linux, macOS and Windows (#45); this dependency lands its cost there.
- **A discovered `ffmpeg`/`ffprobe` binary** — probe `PATH` (and a configured override) at runtime,
  shell out, parse `ffprobe -print_format json`, grab a frame with `ffmpeg -ss … -frames:v 1`.

We already carry one heavy native decode dependency, Assimp ([ADR 0011](0011-assimp-import-backend.md)),
and we know what it costs: it is the reason `.cargo/config.toml` exists at all. Taking a second one
for a poster frame is not a trade we have to make.

## Decision

**Use an `ffmpeg`/`ffprobe` binary discovered at runtime. Do not link libav\*.**

- **Discovery is a runtime probe, not a build-time feature.** `dam-media` looks for `ffprobe`/
  `ffmpeg` on `PATH`, honouring `DAM_FFMPEG`/`DAM_FFPROBE` overrides, and caches the result for the
  process. There is no `video` cargo feature because there is nothing to gate at build time — the
  crate compiles identically with or without ffmpeg installed. This is strictly better than the
  cargo feature the issue proposed: the *same binary* upgrades itself when a user installs ffmpeg,
  with no rebuild.
- **Degradation is graceful and layered**, and each layer is honest about what it knows:
  1. **No ffmpeg at all** → the asset is still detected, catalogued, browsable, searchable by name,
     and playable in the browser (the bytes are served regardless). It shows the typed tile and
     carries only filesystem metadata. This is the floor, and it is a usable floor.
  2. **ffprobe only** → full cheap-tier metadata, still the typed tile.
  3. **ffprobe + ffmpeg** → metadata plus the poster-frame thumbnail.
- **Detection never depends on the probe.** `detect()` is extension-driven and must stay cheap and
  infallible; a missing ffmpeg changes what we *know* about a video, never whether we *see* it.
- **The one place content sniffing is mandatory** is `mp4`/`m4v`/`mov`: those extensions are
  already in the audio decode matrix (`.m4a` is audio-only MP4, and an `.mp4` may legitimately be
  either). Track inspection decides, with a documented fallback when no prober is available —
  `.m4a` stays audio, `.mp4`/`.mov`/`.m4v` default to video.

## Consequences

- **No native build dependency, no licence contamination.** ffmpeg stays an artifact of the *user's*
  system that we invoke, never a library we distribute. That keeps packaging (#45) unchanged for all
  three OSes and keeps our licence story to what we actually ship.
- **We inherit ffmpeg's format coverage for free**, including the long tail (AVI, odd codecs,
  broken-but-playable files) that no pure-Rust stack will match, and it improves when the user
  upgrades their ffmpeg — without a 3DAM release.
- **Process-spawn cost per asset**, roughly 10–50 ms for `ffprobe`. This is a real cost and it is
  paid at the right time: the cheap tier runs at ingest, so a large video-heavy scan is measurably
  slower than an image-only one. It is bounded (one short-lived process per video, inside the
  existing `spawn_blocking` CPU pool, per [ADR 0007](0007-concurrency-tokio-rayon.md)) and it never
  blocks the UI. Metadata for a *catalog* is a one-time cost per asset, not a per-view cost.
- **We must treat the subprocess as untrusted input handling.** Arguments are passed as an argv
  vector (never a shell string), every invocation carries a timeout so a malformed file cannot hang
  a scan worker, and output size is capped. A hostile file is a fail-soft per-asset error
  (golden rule 6), exactly like a corrupt PNG.
- **An external runtime dependency now exists, and the UI has to say so.** A user with videos and no
  ffmpeg gets a working-but-thin experience, and silence there would read as a bug. The absence is
  surfaced (typed tile + an inspector note), not swallowed.
- **Revisit if** poster frames or metadata become hot enough that process spawn dominates ingest, or
  if we ever need *transcoding* (a convert-pipeline target rather than a thumbnail). In-process
  libav becomes defensible when we are decoding whole streams rather than one frame; this ADR is
  the baseline that change would have to beat.

## Alternatives considered

- **`ffmpeg-next` (linked libav\*)** — best fidelity and no external dependency, rejected on build
  weight, cross-compilation burden, and licence/packaging risk, for a payload of one frame plus a
  metadata struct. Reconsider under transcoding, per above.
- **Pure-Rust demux only** — zero dependencies and fully portable, rejected because it forfeits
  poster frames entirely and still would not cover AVI or the damaged-file long tail. Note that this
  option is not *lost*: it is very close to what tier 1 degradation already provides, and a
  pure-Rust container parse could later be added as a better floor when ffmpeg is absent.
- **A bundled ffmpeg binary in our installers** — removes the "user must install it" gap, but
  re-acquires the entire licence and distribution-size problem we just avoided, on three platforms.
  Rejected for v1; a per-platform packaging decision to revisit at #45 if the gap proves painful.
