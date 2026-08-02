// Advanced Search (structured attribute filters). The tags-vs-attributes split made concrete: the
// bounded, extracted metadata the analysis pass stores in the per-media attr columns
// (audio_attr/image_attr/model_attr) is surfaced here as typed dropdown / range / toggle controls,
// contextual to the selected media type, plus a free tag filter. Everything writes the `adv`
// `Filter[]` in view-state, so a whole faceted query stays URL-linkable and composes with the
// sidebar facets and text search. Mirrors the Rust `FacetField` variants one-for-one — keep in sync.

import { useCallback, useEffect, useRef, useState } from "react";
import { SlidersHorizontal, X } from "lucide-react";
import type { FacetField, Filter, MediaType } from "@/api/types";
import { useViewState } from "@/lib/view-state";

/** One structured control. `enum`/`bool` emit a single equality filter; `range` emits a
 *  range/gte/lte depending on which bounds are set; `numEnum` is a dropdown of numeric values. */
type Control =
  | { field: FacetField; label: string; kind: "enum"; options: [string, string][] }
  | { field: FacetField; label: string; kind: "numEnum"; options: [number, string][] }
  | { field: FacetField; label: string; kind: "bool" }
  | { field: FacetField; label: string; kind: "range"; unit?: string; scale?: number };

// Enum option sets mirror the analysis-pass classifiers (dam-media). Keep these in step with the
// string literals the extractors emit (features.rs / audio_features.rs / analysis.rs).
const AUDIO: Control[] = [
  {
    field: "audio_class",
    label: "Type",
    kind: "enum",
    options: [
      ["one_shot", "One-shot"],
      ["loop", "Loop"],
      ["music", "Music"],
      ["sfx", "SFX"],
    ],
  },
  {
    field: "musical_key",
    label: "Key",
    kind: "enum",
    options: ["c", "c#", "d", "d#", "e", "f", "f#", "g", "g#", "a", "a#", "b"].map(
      (k) => [k, k.toUpperCase()] as [string, string],
    ),
  },
  { field: "bpm", label: "BPM", kind: "range", unit: "BPM" },
  { field: "duration", label: "Duration", kind: "range", unit: "s", scale: 1000 },
  {
    field: "sample_rate",
    label: "Sample rate",
    kind: "numEnum",
    options: [
      [22050, "22.05 kHz"],
      [44100, "44.1 kHz"],
      [48000, "48 kHz"],
      [96000, "96 kHz"],
    ],
  },
  {
    field: "channels",
    label: "Channels",
    kind: "numEnum",
    options: [
      [1, "Mono"],
      [2, "Stereo"],
    ],
  },
  {
    field: "bit_depth",
    label: "Bit depth",
    kind: "numEnum",
    options: [
      [16, "16-bit"],
      [24, "24-bit"],
      [32, "32-bit"],
    ],
  },
  { field: "loudness", label: "Loudness", kind: "range", unit: "LUFS" },
  { field: "brightness", label: "Brightness", kind: "range" },
  { field: "harmonicity", label: "Harmonicity", kind: "range" },
];

const IMAGE: Control[] = [
  {
    field: "image_class",
    label: "Type",
    kind: "enum",
    options: [
      ["texture", "Texture"],
      ["sprite", "Sprite"],
      ["photo", "Photo"],
    ],
  },
  {
    field: "tile_class",
    label: "Tiling",
    kind: "enum",
    options: [
      ["seamless", "Seamless"],
      ["tiled", "Tiled"],
      ["non_tiling", "Non-tiling"],
    ],
  },
  { field: "width", label: "Width", kind: "range", unit: "px" },
  { field: "height", label: "Height", kind: "range", unit: "px" },
  { field: "tileability", label: "Tileability", kind: "range" },
  { field: "has_alpha", label: "Alpha channel", kind: "bool" },
];

const MODEL: Control[] = [
  {
    field: "model_class",
    label: "Complexity",
    kind: "enum",
    options: [
      ["prop_lowpoly", "Low-poly"],
      ["prop", "Prop"],
      ["prop_highpoly", "High-poly"],
    ],
  },
  { field: "tri_count", label: "Triangles", kind: "range" },
  { field: "vertex_count", label: "Vertices", kind: "range" },
  { field: "mesh_count", label: "Meshes", kind: "range" },
  { field: "material_count", label: "Materials", kind: "range" },
  { field: "texture_count", label: "Textures", kind: "range" },
  { field: "has_rig", label: "Rigged", kind: "bool" },
  { field: "has_animation", label: "Animated", kind: "bool" },
  { field: "has_uv", label: "UV mapped", kind: "bool" },
];

const VIDEO: Control[] = [
  {
    field: "video_class",
    label: "Length",
    kind: "enum",
    options: [
      ["sting", "Sting"],
      ["clip", "Clip"],
      ["cutscene", "Cutscene"],
    ],
  },
  { field: "duration", label: "Duration", kind: "range", unit: "ms" },
  { field: "width", label: "Width", kind: "range", unit: "px" },
  { field: "height", label: "Height", kind: "range", unit: "px" },
  { field: "fps", label: "Frame rate", kind: "range", unit: "fps" },
  { field: "bitrate", label: "Bitrate", kind: "range", unit: "bps" },
  { field: "has_audio", label: "Has audio", kind: "bool" },
];

const DOCUMENT: Control[] = [
  {
    field: "document_class",
    label: "Kind",
    kind: "enum",
    options: [
      ["license", "Licence"],
      ["readme", "Readme"],
      ["changelog", "Changelog"],
      ["receipt", "Receipt"],
      ["document", "Other"],
    ],
  },
  { field: "page_count", label: "Pages", kind: "range" },
  { field: "word_count", label: "Words", kind: "range" },
];

const CATALOG: Record<MediaType, Control[]> = {
  audio: AUDIO,
  image: IMAGE,
  model: MODEL,
  video: VIDEO,
  document: DOCUMENT,
};

/** Replace (or clear, when `filter` is null) the single filter targeting `field`. */
function withFilter(adv: Filter[], field: FacetField, filter: Filter | null): Filter[] {
  const rest = adv.filter((f) => f.field !== field);
  return filter ? [...rest, filter] : rest;
}

function findByField(adv: Filter[], field: FacetField): Filter | undefined {
  return adv.find((f) => f.field === field);
}

/** Read a range control's current [min, max] back out of its filter, in display units. */
function readRange(adv: Filter[], field: FacetField, scale = 1): [string, string] {
  const f = findByField(adv, field);
  if (!f) return ["", ""];
  const u = (n: number) => String(n / scale);
  if (f.op === "range" && "range" in f.value) return [u(f.value.range[0]), u(f.value.range[1])];
  if (f.op === "gte" && "num" in f.value) return [u(f.value.num), ""];
  if (f.op === "lte" && "num" in f.value) return ["", u(f.value.num)];
  return ["", ""];
}

/** Build a range/gte/lte filter (or null) from the raw min/max inputs, applying the unit scale. */
function rangeFilter(field: FacetField, min: string, max: string, scale = 1): Filter | null {
  const lo = min.trim() === "" ? null : Number(min) * scale;
  const hi = max.trim() === "" ? null : Number(max) * scale;
  if (lo != null && Number.isNaN(lo)) return null;
  if (hi != null && Number.isNaN(hi)) return null;
  if (lo != null && hi != null) return { field, op: "range", value: { range: [lo, hi] } };
  if (lo != null) return { field, op: "gte", value: { num: lo } };
  if (hi != null) return { field, op: "lte", value: { num: hi } };
  return null;
}

function ControlRow({
  control,
  adv,
  onChange,
}: {
  control: Control;
  adv: Filter[];
  onChange: (next: Filter[]) => void;
}) {
  const { field, label } = control;
  const set = (filter: Filter | null) => onChange(withFilter(adv, field, filter));

  if (control.kind === "enum") {
    const cur = findByField(adv, field);
    const value = cur && "str" in cur.value ? cur.value.str : "";
    return (
      <Labeled label={label}>
        <select
          className="field w-full"
          aria-label={label}
          value={value}
          onChange={(e) =>
            set(e.target.value ? { field, op: "eq", value: { str: e.target.value } } : null)
          }
        >
          <option value="">Any</option>
          {control.options.map(([v, l]) => (
            <option key={v} value={v}>
              {l}
            </option>
          ))}
        </select>
      </Labeled>
    );
  }

  if (control.kind === "numEnum") {
    const cur = findByField(adv, field);
    const value = cur && "num" in cur.value ? String(cur.value.num) : "";
    return (
      <Labeled label={label}>
        <select
          className="field w-full"
          aria-label={label}
          value={value}
          onChange={(e) =>
            set(e.target.value ? { field, op: "eq", value: { num: Number(e.target.value) } } : null)
          }
        >
          <option value="">Any</option>
          {control.options.map(([v, l]) => (
            <option key={v} value={v}>
              {l}
            </option>
          ))}
        </select>
      </Labeled>
    );
  }

  if (control.kind === "bool") {
    const cur = findByField(adv, field);
    const value = cur && "bool" in cur.value ? (cur.value.bool ? "yes" : "no") : "";
    return (
      <Labeled label={label}>
        <select
          className="field w-full"
          aria-label={label}
          value={value}
          onChange={(e) =>
            set(e.target.value ? { field, op: "eq", value: { bool: e.target.value === "yes" } } : null)
          }
        >
          <option value="">Any</option>
          <option value="yes">Yes</option>
          <option value="no">No</option>
        </select>
      </Labeled>
    );
  }

  // range
  const [min, max] = readRange(adv, field, control.scale ?? 1);
  const commit = (lo: string, hi: string) => set(rangeFilter(field, lo, hi, control.scale ?? 1));
  return (
    <Labeled label={control.unit ? `${label} (${control.unit})` : label}>
      <div className="flex items-center gap-1">
        <input
          type="number"
          inputMode="decimal"
          aria-label={`Minimum ${label}`}
          className="field w-full"
          placeholder="min"
          defaultValue={min}
          key={`min-${min}`}
          onBlur={(e) => commit(e.target.value, max)}
        />
        <span className="text-fg-dim">–</span>
        <input
          type="number"
          inputMode="decimal"
          aria-label={`Maximum ${label}`}
          className="field w-full"
          placeholder="max"
          defaultValue={max}
          key={`max-${max}`}
          onBlur={(e) => commit(min, e.target.value)}
        />
      </div>
    </Labeled>
  );
}

function Labeled({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center gap-2 text-[11px] text-fg-muted">
      <span className="w-24 shrink-0">{label}</span>
      {children}
    </div>
  );
}

/** Free tag filter (multi): each accepted tag AND-s a `tag` equality onto the query. Tags power
 *  search now that they've left the sidebar; this is where you pin one as a hard filter. */
function TagFilter({ adv, onChange }: { adv: Filter[]; onChange: (next: Filter[]) => void }) {
  const [text, setText] = useState("");
  const tags = adv
    .filter((f) => f.field === "tag" && "str" in f.value)
    .map((f) => (f.value as { str: string }).str);
  const add = (name: string) => {
    const n = name.trim();
    if (!n || tags.some((t) => t.toLowerCase() === n.toLowerCase())) return;
    onChange([...adv, { field: "tag", op: "eq", value: { str: n } }]);
    setText("");
  };
  const remove = (name: string) =>
    onChange(adv.filter((f) => !(f.field === "tag" && "str" in f.value && f.value.str === name)));
  return (
    <div className="flex flex-col gap-1">
      <span className="text-[11px] text-fg-muted">Tags</span>
      <input
        className="field w-full"
        aria-label="Add tag filter"
        placeholder="Add a tag filter, then Enter"
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            add(text);
          }
        }}
      />
      {tags.length > 0 && (
        <div className="flex flex-wrap gap-1 pt-0.5">
          {tags.map((t) => (
            <span
              key={t}
              className="inline-flex items-center gap-1 rounded border border-accent bg-accent-muted px-1.5 py-0.5 text-[10px] text-accent"
            >
              {t}
              <button
                onClick={() => remove(t)}
                aria-label={`Remove tag filter ${t}`}
                className="hover:text-fg coarse:min-h-11 coarse:min-w-11"
              >
                <X size={11} />
              </button>
            </span>
          ))}
        </div>
      )}
    </div>
  );
}

/** The "Filters" toolbar button + its popover panel. Contextual to the active media type: pick a
 *  media in the sidebar and its structured controls appear. Tag filters are always available. */
export function AdvancedSearch() {
  const { state, patch } = useViewState();
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const panelRef = useRef<HTMLDivElement>(null);
  const adv = state.adv;
  const setAdv = (next: Filter[]) => patch({ adv: next, collection: null });

  const close = useCallback((restoreFocus: boolean) => {
    setOpen(false);
    if (restoreFocus) requestAnimationFrame(() => triggerRef.current?.focus());
  }, []);

  // A non-modal dialog popover: focus enters its labelled panel, click-away dismisses without
  // stealing focus from the clicked control, and keyboard focus leaving the popover closes it.
  useEffect(() => {
    if (!open) return;
    const onPointerDown = (event: PointerEvent) => {
      if (!rootRef.current?.contains(event.target as Node)) close(false);
    };
    document.addEventListener("pointerdown", onPointerDown, true);
    requestAnimationFrame(() => panelRef.current?.focus());
    return () => document.removeEventListener("pointerdown", onPointerDown, true);
  }, [close, open]);

  const controls = state.media ? CATALOG[state.media] : null;
  const activeCount = adv.length;

  return (
    <div
      ref={rootRef}
      className="relative shrink-0"
      onBlur={(event) => {
        const next = event.relatedTarget;
        if (next instanceof Node && rootRef.current?.contains(next)) return;
        close(false);
      }}
    >
      <button
        ref={triggerRef}
        className="btn flex items-center gap-1 px-1.5 py-1 coarse:min-h-11"
        title="Advanced filters"
        aria-label="Advanced filters"
        aria-expanded={open}
        aria-haspopup="dialog"
        aria-controls="advanced-search-popover"
        onClick={() => setOpen((o) => !o)}
      >
        <SlidersHorizontal size={14} />
        {activeCount > 0 && (
          <span className="rounded-full bg-accent px-1 text-[10px] leading-4 font-semibold text-black tabular-nums">
            {activeCount}
          </span>
        )}
      </button>
      {open && (
        <div
          ref={panelRef}
          id="advanced-search-popover"
          role="dialog"
          aria-modal="false"
          aria-labelledby="advanced-search-title"
          tabIndex={-1}
          onKeyDown={(event) => {
            if (event.key !== "Escape") return;
            event.preventDefault();
            event.stopPropagation();
            close(true);
          }}
          className="absolute right-0 z-50 mt-1 max-h-[70vh] w-80 overflow-y-auto rounded-md border border-border bg-surface p-3 shadow-xl"
        >
          <div className="mb-2 flex items-center justify-between">
            <h2 id="advanced-search-title" className="text-xs font-semibold text-fg">
              Advanced filters
            </h2>
            <div className="flex items-center gap-2">
              {activeCount > 0 && (
                <button
                  className="text-[11px] text-fg-dim hover:text-accent"
                  onClick={() => setAdv([])}
                >
                  Clear advanced
                </button>
              )}
              <button
                type="button"
                className="text-fg-dim hover:text-fg"
                aria-label="Close advanced filters"
                onClick={() => close(true)}
              >
                <X size={13} />
              </button>
            </div>
          </div>

          {controls ? (
            <div className="flex flex-col gap-1.5">
              {controls.map((c) => (
                <ControlRow key={c.field} control={c} adv={adv} onChange={setAdv} />
              ))}
            </div>
          ) : (
            <p className="text-[11px] text-fg-dim italic">
              Pick a media type (Audio, Images, 3D Models) to filter on its properties — BPM, key,
              dimensions, triangle count, and more.
            </p>
          )}

          <div className="my-2 border-t border-border" />
          <TagFilter adv={adv} onChange={setAdv} />
        </div>
      )}
    </div>
  );
}
