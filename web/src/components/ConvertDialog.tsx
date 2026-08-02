// Convert dialog (issues #5/#114). Submits a background job, then closes; progress, cancellation,
// and the durable per-item report live in the shared status/history surfaces. One
// target per request — inputs of the other media type come back `unsupported` (fail-soft), so the
// dialog defaults the target to the media most of the selection is. Non-destructive by construction:
// outputs go under the chosen dir, never a source (the backend enforces §5.1).

import { useMemo, useState } from "react";
import { FileCog } from "lucide-react";
import { useConvert } from "@/api/queries";
import { ApiError } from "@/api/client";
import { Modal } from "@/lib/dialogs";
import type {
  AssetSummary,
  CollisionRule,
  ConvertTarget,
} from "@/api/types";

const IMAGE_FORMATS = ["png", "jpg", "webp", "bmp", "tga", "tiff", "gif"];
const AUDIO_FORMATS = ["wav"];
/** 3D containers (issue #49). Text glTF and OBJ are published with their `.bin`/`.mtl` companion. */
const MODEL_FORMATS = ["glb", "gltf", "obj"];
const LOSSY = new Set(["jpg", "webp"]);

/** The media classes `convert` can target. Not every `MediaType` — video and documents have no
 *  encode path (PRODUCT_SPEC §9 phase 2b is deliberately shallow for both). */
type TargetMedia = "image" | "audio" | "model";

function formatsFor(m: TargetMedia): string[] {
  return m === "image" ? IMAGE_FORMATS : m === "model" ? MODEL_FORMATS : AUDIO_FORMATS;
}

function defaultFormat(m: TargetMedia): string {
  return formatsFor(m)[0];
}
const COLLISION: { value: CollisionRule; label: string }[] = [
  { value: "fail", label: "Fail on existing" },
  { value: "suffix", label: "Add -N suffix" },
  { value: "skip", label: "Skip existing" },
  { value: "overwrite", label: "Overwrite" },
];

export function ConvertDialog({
  assets,
  onClose,
}: {
  assets: AssetSummary[];
  onClose: () => void;
}) {
  const run = useConvert();
  const [err, setErr] = useState<string | null>(null);

  // Default the target media to whichever the selection mostly is.
  const defaultMedia = useMemo<TargetMedia>(() => {
    const counts: Record<TargetMedia, number> = {
      image: assets.filter((a) => a.media === "image").length,
      audio: assets.filter((a) => a.media === "audio").length,
      model: assets.filter((a) => a.media === "model").length,
    };
    // Whichever the selection mostly is; ties fall to image, which is the common case.
    return (Object.keys(counts) as TargetMedia[]).reduce((best, m) =>
      counts[m] > counts[best] ? m : best,
    );
  }, [assets]);

  const [media, setMedia] = useState<TargetMedia>(defaultMedia);
  const [format, setFormat] = useState(defaultFormat(defaultMedia));
  const [maxEdge, setMaxEdge] = useState("");
  const [quality, setQuality] = useState("");
  const [optimize, setOptimize] = useState(false);
  const [outputDir, setOutputDir] = useState("");
  const [collision, setCollision] = useState<CollisionRule>("fail");
  const [dryRun, setDryRun] = useState(true);

  const switchMedia = (m: TargetMedia) => {
    setMedia(m);
    setFormat(defaultFormat(m));
  };

  // How many inputs match the chosen target media (the rest will report `unsupported`).
  const matching = assets.filter((a) => a.media === media).length;

  const submit = () => {
    setErr(null);
    if (!outputDir.trim()) return setErr("An output directory is required.");
    const target: ConvertTarget =
      media === "image"
        ? {
            media: "image",
            format,
            max_edge: maxEdge ? Number(maxEdge) : null,
            quality: quality && LOSSY.has(format) ? Number(quality) : null,
          }
        : media === "model"
          ? { media: "model", format, optimize }
          : { media: "audio", format };
    run.mutate(
      {
        inputs: assets.map((a) => a.id),
        target,
        output_dir: outputDir.trim(),
        dry_run: dryRun,
        on_collision: collision,
      },
      {
        onSuccess: onClose,
        onError: (e) => setErr(e instanceof ApiError ? e.message : String(e)),
      },
    );
  };

  return (
    <Modal
      title={`Convert ${assets.length} asset${assets.length === 1 ? "" : "s"}`}
      icon={<FileCog size={15} />}
      labelledBy="convert-dialog-title"
      onClose={onClose}
      wide
      scroll
    >
      <div className="flex flex-col gap-3">
            {/* target media */}
            <div>
              <label className="mb-1 block text-[11px] text-fg-muted">Target media</label>
              <div className="flex overflow-hidden rounded border border-border">
                {(["image", "audio", "model"] as const).map((m) => (
                  <button
                    key={m}
                    onClick={() => switchMedia(m)}
                    className="flex-1 px-3 py-1 text-xs capitalize coarse:min-h-11"
                    style={{
                      background: media === m ? "var(--color-accent)" : "var(--color-surface-2)",
                      color: media === m ? "var(--color-accent-fg)" : "var(--color-fg-muted)",
                    }}
                  >
                    {m}
                  </button>
                ))}
              </div>
              {matching < assets.length && (
                <p className="mt-1 text-[10px] text-warn">
                  {assets.length - matching} non-{media} item
                  {assets.length - matching === 1 ? "" : "s"} will report as unsupported.
                </p>
              )}
            </div>

            {/* format */}
            <div className="flex gap-2">
              <div className="flex-1">
                <label className="mb-1 block text-[11px] text-fg-muted">Format</label>
                <select
                  className="field"
                  aria-label="Output format"
                  value={format}
                  onChange={(e) => setFormat(e.target.value)}
                >
                  {formatsFor(media).map((f) => (
                    <option key={f} value={f}>
                      {f.toUpperCase()}
                    </option>
                  ))}
                </select>
              </div>
              {media === "image" && (
                <div className="w-24">
                  <label className="mb-1 block text-[11px] text-fg-muted">Max edge</label>
                  <input
                    className="field"
                    type="number"
                    min={1}
                    placeholder="—"
                    value={maxEdge}
                    onChange={(e) => setMaxEdge(e.target.value)}
                  />
                </div>
              )}
              {media === "image" && LOSSY.has(format) && (
                <div className="w-20">
                  <label className="mb-1 block text-[11px] text-fg-muted">Quality</label>
                  <input
                    className="field"
                    type="number"
                    min={1}
                    max={100}
                    placeholder="—"
                    value={quality}
                    onChange={(e) => setQuality(e.target.value)}
                  />
                </div>
              )}
            </div>

            {/* 3D: mesh optimisation (issue #49). Opt-in, because it collapses the node graph —
                the geometry is preserved, the names and hierarchy around it may not be. */}
            {media === "model" && (
              <div>
                <label className="flex items-center gap-1.5 text-[11px] text-fg-muted coarse:min-h-11">
                  <input
                    type="checkbox"
                    checked={optimize}
                    onChange={(e) => setOptimize(e.target.checked)}
                  />
                  Optimise + compress mesh
                </label>
                <p className="mt-1 text-[10px] text-fg-dim">
                  Merges redundant materials and meshes, drops degenerate faces, and re-joins shared
                  vertices — fewer draw calls for the same model. GLB/glTF also uses Draco geometry
                  compression. Node names and hierarchy may not survive; the original is unchanged.
                </p>
              </div>
            )}

            {/* output dir */}
            <div>
              <label className="mb-1 block text-[11px] text-fg-muted">Output directory</label>
              <input
                className="field"
                placeholder="/home/me/converted"
                value={outputDir}
                onChange={(e) => setOutputDir(e.target.value)}
                spellCheck={false}
              />
              <p className="mt-1 text-[10px] text-fg-dim">
                Must be outside any registered source — 3DAM never writes into a source.
              </p>
            </div>

            {/* collision + dry run */}
            <div className="flex items-end gap-2">
              <div className="flex-1">
                <label className="mb-1 block text-[11px] text-fg-muted">On collision</label>
                <select
                  className="field"
                  aria-label="On collision"
                  value={collision}
                  onChange={(e) => setCollision(e.target.value as CollisionRule)}
                >
                  {COLLISION.map((c) => (
                    <option key={c.value} value={c.value}>
                      {c.label}
                    </option>
                  ))}
                </select>
              </div>
              <label className="flex items-center gap-1.5 pb-1.5 text-[11px] text-fg-muted">
                <input
                  type="checkbox"
                  checked={dryRun}
                  onChange={(e) => setDryRun(e.target.checked)}
                />
                Dry run (plan only)
              </label>
            </div>

            {err && <p className="text-[11px] text-danger">{err}</p>}

            <div className="flex justify-end gap-2">
              <button className="btn" onClick={onClose}>
                Cancel
              </button>
              <button
                className="btn btn-accent"
                onClick={submit}
                disabled={run.isPending}
              >
                {run.isPending ? "Submitting…" : dryRun ? "Plan in background" : "Convert in background"}
              </button>
            </div>
      </div>
    </Modal>
  );
}
