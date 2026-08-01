// Convert dialog (issue #5). Wires POST /api/v1/convert: pick a media-typed target (image or audio)
// + format/options + an output directory, run (or dry-run) it, and show the per-item report. One
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
  ConvertReport,
  ConvertTarget,
  Disposition,
} from "@/api/types";
import { bytes } from "@/lib/format";

const IMAGE_FORMATS = ["png", "jpg", "webp", "bmp", "tga", "tiff", "gif"];
const AUDIO_FORMATS = ["wav"];
/** 3D containers (issue #49). `glb` only: it is self-contained, where `gltf`/`obj` emit sidecar
 *  files the convert pipeline cannot yet write as one output — the server refuses those by name. */
const MODEL_FORMATS = ["glb"];
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

const DISPOSITION_COLOR: Record<Disposition, string> = {
  done: "var(--color-lic-permissive)",
  write: "var(--color-accent)",
  skipped: "var(--color-fg-dim)",
  collision: "var(--color-warn)",
  unsupported: "var(--color-fg-dim)",
  failed: "var(--color-danger)",
};

export function ConvertDialog({
  assets,
  onClose,
}: {
  assets: AssetSummary[];
  onClose: () => void;
}) {
  const run = useConvert();
  const [report, setReport] = useState<ConvertReport | null>(null);
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
        onSuccess: (r) => setReport(r),
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
      {report ? (
          <ReportView report={report} onClose={onClose} onAgain={() => setReport(null)} />
        ) : (
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
                  Optimise mesh
                </label>
                <p className="mt-1 text-[10px] text-fg-dim">
                  Merges redundant materials and meshes, drops degenerate faces, and re-joins shared
                  vertices — fewer draw calls for the same model. Node names and hierarchy may not
                  survive; the original is never modified.
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
                {run.isPending ? "Working…" : dryRun ? "Plan" : "Convert"}
              </button>
            </div>
          </div>
        )}
    </Modal>
  );
}

function ReportView({
  report,
  onClose,
  onAgain,
}: {
  report: ConvertReport;
  onClose: () => void;
  onAgain: () => void;
}) {
  return (
    <div className="flex flex-col gap-3">
      <div className="rounded border border-border bg-bg p-2.5 text-xs">
        <p className="text-fg">
          {report.dry_run ? "Planned" : "Converted"} → {report.output_dir}
        </p>
        <div className="mt-1.5 flex flex-wrap gap-x-3 gap-y-1 text-[11px]">
          <Stat label={report.dry_run ? "would write" : "done"} n={report.done} tone="ok" />
          {report.collisions > 0 && <Stat label="collisions" n={report.collisions} tone="warn" />}
          {report.unsupported > 0 && <Stat label="unsupported" n={report.unsupported} />}
          {report.failed > 0 && <Stat label="failed" n={report.failed} tone="bad" />}
          {!report.dry_run && report.total_output_bytes > 0 && (
            <span className="text-fg-dim">
              {bytes(report.total_input_bytes)} → {bytes(report.total_output_bytes)}
            </span>
          )}
        </div>
      </div>

      <div className="max-h-56 overflow-y-auto rounded border border-border">
        {report.items.map((it) => (
          <div
            key={it.input}
            className="flex items-center justify-between gap-2 border-b border-border px-2 py-1 text-[11px] last:border-0"
          >
            <span className="min-w-0 flex-1 truncate text-fg-muted" title={it.planned_output}>
              {it.planned_output.split(/[/\\]/).pop() || it.input_path}
            </span>
            {it.error ? (
              <span className="max-w-[45%] truncate text-danger" title={it.error}>
                {it.error}
              </span>
            ) : (
              <span
                className="shrink-0 rounded px-1.5 py-0.5 text-[10px]"
                style={{
                  color: DISPOSITION_COLOR[it.disposition],
                  background: "color-mix(in srgb, currentColor 12%, transparent)",
                }}
              >
                {it.disposition}
                {it.ratio != null ? ` · ${Math.round(it.ratio * 100)}%` : ""}
              </span>
            )}
          </div>
        ))}
      </div>

      <div className="flex justify-end gap-2">
        <button className="btn" onClick={onAgain}>
          Back
        </button>
        <button className="btn btn-accent" onClick={onClose}>
          Done
        </button>
      </div>
    </div>
  );
}

function Stat({ label, n, tone }: { label: string; n: number; tone?: "ok" | "warn" | "bad" }) {
  const color =
    tone === "ok"
      ? "var(--color-lic-permissive)"
      : tone === "warn"
        ? "var(--color-warn)"
        : tone === "bad"
          ? "var(--color-danger)"
          : "var(--color-fg-muted)";
  return (
    <span style={{ color }}>
      <span className="font-semibold tabular-nums">{n}</span> {label}
    </span>
  );
}
