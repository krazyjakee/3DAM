// Interactive tile preview (issue #58). Wraps a texture across a rotating 3D cube — and a flat NxN
// grid — so you can eyeball whether it tiles seamlessly (pairs with the analysis seamlessness score,
// #57). Pure browser primitives: CSS 3D transforms with the texture as a repeating background, no
// WASM and no extra deps (the WASM viewer has no texture sampler). Drag to rotate; the cube auto-spins
// until you touch it. A density control sets how many times the texture repeats per face.

import { useEffect, useRef, useState } from "react";
import { Box, Grid3x3, Pause, Play, X } from "lucide-react";
import { useFocusTrap } from "@/lib/use-focus-trap";

const FACES = [
  { t: "translateZ(var(--h))" }, // front
  { t: "rotateY(180deg) translateZ(var(--h))" }, // back
  { t: "rotateY(90deg) translateZ(var(--h))" }, // right
  { t: "rotateY(-90deg) translateZ(var(--h))" }, // left
  { t: "rotateX(90deg) translateZ(var(--h))" }, // top
  { t: "rotateX(-90deg) translateZ(var(--h))" }, // bottom
];

const SIZE = 240; // cube edge (px)

export function TilePreview({
  src,
  name,
  onClose,
}: {
  src: string;
  name: string;
  onClose: () => void;
}) {
  const [mode, setMode] = useState<"cube" | "flat">("cube");
  const [repeat, setRepeat] = useState(2); // texture repeats per face edge
  const [spin, setSpin] = useState(true);
  const [rot, setRot] = useState({ x: -18, y: 24 });
  const drag = useRef<{ x: number; y: number } | null>(null);

  const dialogRef = useFocusTrap<HTMLDivElement>(true);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  // Auto-spin around Y until the user grabs it (rAF so a drag blends in smoothly).
  useEffect(() => {
    if (!spin || mode !== "cube") return;
    let raf = 0;
    const tick = () => {
      if (!drag.current) setRot((r) => ({ ...r, y: r.y + 0.3 }));
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [spin, mode]);

  const onPointerDown = (e: React.PointerEvent) => {
    drag.current = { x: e.clientX, y: e.clientY };
    (e.target as HTMLElement).setPointerCapture?.(e.pointerId);
  };
  const onPointerMove = (e: React.PointerEvent) => {
    if (!drag.current) return;
    const dx = e.clientX - drag.current.x;
    const dy = e.clientY - drag.current.y;
    drag.current = { x: e.clientX, y: e.clientY };
    setRot((r) => ({ x: clamp(r.x - dy * 0.5, -89, 89), y: r.y + dx * 0.5 }));
  };
  const onPointerUp = () => {
    drag.current = null;
  };

  // A repeating-background face: `background-size` = 100/repeat% gives `repeat` tiles per edge.
  const faceBg: React.CSSProperties = {
    backgroundImage: `url(${src})`,
    backgroundRepeat: "repeat",
    backgroundSize: `${100 / repeat}% ${100 / repeat}%`,
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/70 p-4"
      onClick={onClose}
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby="tile-preview-title"
        className="w-full max-w-[420px] rounded-lg border border-border bg-surface p-4 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-3 flex items-center justify-between">
          <h2 id="tile-preview-title" className="flex min-w-0 items-center gap-2 text-sm font-semibold">
            <Box size={15} /> <span className="truncate">Tiling — {name}</span>
          </h2>
          <button
            className="flex items-center justify-center text-fg-dim hover:text-fg coarse:min-h-11 coarse:min-w-11"
            onClick={onClose}
            aria-label="Close"
          >
            <X size={16} />
          </button>
        </div>

        {/* stage */}
        <div
          className="relative flex touch-none items-center justify-center overflow-hidden rounded border border-border bg-bg select-none"
          style={{ height: SIZE + 60, perspective: mode === "cube" ? "900px" : undefined }}
          onPointerDown={mode === "cube" ? onPointerDown : undefined}
          onPointerMove={mode === "cube" ? onPointerMove : undefined}
          onPointerUp={onPointerUp}
          onPointerCancel={onPointerUp}
        >
          {mode === "cube" ? (
            <div
              style={
                {
                  width: SIZE,
                  height: SIZE,
                  transformStyle: "preserve-3d",
                  transform: `rotateX(${rot.x}deg) rotateY(${rot.y}deg)`,
                  ["--h" as string]: `${SIZE / 2}px`,
                  cursor: "grab",
                } as React.CSSProperties
              }
            >
              {FACES.map((f, i) => (
                <div
                  key={i}
                  className="absolute inset-0 border border-black/40"
                  style={{ ...faceBg, transform: f.t }}
                />
              ))}
            </div>
          ) : (
            // Flat: a single tile fills the stage, repeated — the plainest seam check.
            <div className="h-full w-full" style={faceBg} />
          )}
        </div>

        {/* controls */}
        <div className="mt-3 flex flex-col gap-3">
          <div className="flex items-center gap-2">
            <div className="flex overflow-hidden rounded border border-border">
              {(
                [
                  ["cube", <Box size={13} key="c" />, "Cube"],
                  ["flat", <Grid3x3 size={13} key="g" />, "Flat"],
                ] as const
              ).map(([m, icon, label]) => (
                <button
                  key={m}
                  onClick={() => setMode(m)}
                  className="flex items-center gap-1.5 px-3 py-1 text-xs coarse:min-h-11"
                  style={{
                    background: mode === m ? "var(--color-accent)" : "var(--color-surface-2)",
                    color: mode === m ? "var(--color-accent-fg)" : "var(--color-fg-muted)",
                  }}
                >
                  {icon} {label}
                </button>
              ))}
            </div>
            {mode === "cube" && (
              <button
                onClick={() => setSpin((s) => !s)}
                className="btn flex items-center gap-1.5 coarse:min-h-11"
                title={spin ? "Pause rotation" : "Auto-rotate"}
              >
                {spin ? <Pause size={13} /> : <Play size={13} />}
                {spin ? "Pause" : "Spin"}
              </button>
            )}
          </div>

          <label className="flex items-center gap-2 text-[11px] text-fg-muted">
            <span className="w-14 shrink-0">Repeat ×{repeat}</span>
            <input
              type="range"
              min={1}
              max={6}
              step={1}
              value={repeat}
              onChange={(e) => setRepeat(Number(e.target.value))}
              className="h-1 flex-1 cursor-pointer coarse:h-2"
              style={{ accentColor: "var(--color-accent)" }}
              aria-label="Tiles per face"
            />
          </label>
          <p className="text-[10px] text-fg-dim">
            {mode === "cube"
              ? "Drag to rotate. Visible seams between repeats mean the texture isn’t seamless."
              : "The texture repeated as a flat sheet — look for seams along the tile edges."}
          </p>
        </div>
      </div>
    </div>
  );
}

function clamp(v: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, v));
}
