// Zoom + pan image viewer (issue #17). Scroll / pinch to zoom, drag to pan, with fit-to-window and
// 1:1 (pixel-accurate) resets — for inspecting texture detail / tileability. Fed the full-resolution
// content image (not the thumbnail) so 1:1 is truly pixel-accurate. Works on coarse pointers: drag
// pans, two-finger pinch zooms (responsive/touch pass). Transform is visual only (no reflow), so the
// image's layout size stays the fitted size and 1:1 is naturalWidth / fittedWidth.

import { useCallback, useEffect, useRef, useState } from "react";
import { Maximize2, Minus, Plus, Scan } from "lucide-react";

const MIN_SCALE = 1; // 1 = fit-to-window (object-contain baseline)
const MAX_SCALE = 16;

const clamp = (n: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, n));

interface Transform {
  scale: number;
  tx: number;
  ty: number;
}
const FIT: Transform = { scale: 1, tx: 0, ty: 0 };

export function ImageViewer({ src, alt }: { src: string; alt: string }) {
  const boxRef = useRef<HTMLDivElement>(null);
  const imgRef = useRef<HTMLImageElement>(null);
  const [t, setT] = useState<Transform>(FIT);
  const [failed, setFailed] = useState(false);

  // Reset when the image changes (keyed by src at the call site remounts anyway, but be safe).
  useEffect(() => {
    setT(FIT);
    setFailed(false);
  }, [src]);

  /** Re-scale about the point at container-coords (px,py), keeping it stationary. `next` maps the
   *  current scale to the target scale. Origin is the box centre (`transform-origin: center`). */
  const zoomAtPoint = useCallback(
    (next: (curScale: number) => number, px: number, py: number) => {
      const box = boxRef.current;
      if (!box) return;
      const r = box.getBoundingClientRect();
      const cx = r.width / 2;
      const cy = r.height / 2;
      setT((cur) => {
        const s2 = clamp(next(cur.scale), MIN_SCALE, MAX_SCALE);
        const k = s2 / cur.scale;
        const tx = px - cx - k * (px - cx - cur.tx);
        const ty = py - cy - k * (py - cy - cur.ty);
        return s2 === MIN_SCALE ? FIT : { scale: s2, tx, ty };
      });
    },
    [],
  );
  const zoomTo = useCallback(
    (scale: number, px: number, py: number) => zoomAtPoint(() => scale, px, py),
    [zoomAtPoint],
  );

  // Wheel zoom — native non-passive listener so we can preventDefault the page scroll.
  useEffect(() => {
    const box = boxRef.current;
    if (!box) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const r = box.getBoundingClientRect();
      const factor = e.deltaY < 0 ? 1.15 : 1 / 1.15;
      zoomAtPoint((s) => s * factor, e.clientX - r.left, e.clientY - r.top);
    };
    box.addEventListener("wheel", onWheel, { passive: false });
    return () => box.removeEventListener("wheel", onWheel);
  }, [zoomAtPoint]);

  // Pointer drag-to-pan + two-pointer pinch-zoom.
  const pointers = useRef(new Map<number, { x: number; y: number }>());
  const pinchStart = useRef<{ dist: number; scale: number } | null>(null);

  const onPointerDown = (e: React.PointerEvent) => {
    (e.target as Element).setPointerCapture?.(e.pointerId);
    pointers.current.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (pointers.current.size === 2) {
      const [a, b] = [...pointers.current.values()];
      pinchStart.current = { dist: Math.hypot(a.x - b.x, a.y - b.y), scale: t.scale };
    }
  };

  const onPointerMove = (e: React.PointerEvent) => {
    const prev = pointers.current.get(e.pointerId);
    if (!prev) return;
    const next = { x: e.clientX, y: e.clientY };
    pointers.current.set(e.pointerId, next);

    if (pointers.current.size >= 2 && pinchStart.current) {
      // Pinch: scale by the change in finger distance, zooming about the finger midpoint.
      const [a, b] = [...pointers.current.values()];
      const dist = Math.hypot(a.x - b.x, a.y - b.y);
      const box = boxRef.current;
      if (!box || dist === 0) return;
      const r = box.getBoundingClientRect();
      const mx = (a.x + b.x) / 2 - r.left;
      const my = (a.y + b.y) / 2 - r.top;
      zoomTo((pinchStart.current.scale * dist) / pinchStart.current.dist, mx, my);
      return;
    }
    // Single pointer: pan (only meaningful when zoomed in).
    if (t.scale > 1) {
      const dx = next.x - prev.x;
      const dy = next.y - prev.y;
      setT((cur) => ({ ...cur, tx: cur.tx + dx, ty: cur.ty + dy }));
    }
  };

  const endPointer = (e: React.PointerEvent) => {
    pointers.current.delete(e.pointerId);
    if (pointers.current.size < 2) pinchStart.current = null;
  };

  const zoomStep = (factor: number) => {
    const box = boxRef.current;
    if (!box) return;
    const r = box.getBoundingClientRect();
    // Functional form so repeated taps compound off the live scale, not a stale render closure.
    zoomAtPoint((s) => s * factor, r.width / 2, r.height / 2);
  };

  const oneToOne = () => {
    const img = imgRef.current;
    const box = boxRef.current;
    if (!img || !box) return;
    // Layout (fitted) size is unaffected by the transform, so this ratio is exact pixel scale.
    const fittedW = img.clientWidth || 1;
    zoomTo(clamp(img.naturalWidth / fittedW, MIN_SCALE, MAX_SCALE), box.clientWidth / 2, box.clientHeight / 2);
  };

  if (failed) {
    return (
      <div className="flex h-full w-full items-center justify-center bg-bg text-[11px] text-fg-dim">
        Preview unavailable
      </div>
    );
  }

  return (
    <div
      ref={boxRef}
      className="relative h-full w-full touch-none overflow-hidden bg-bg select-none"
      style={{ cursor: t.scale > 1 ? "grab" : "default" }}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={endPointer}
      onPointerCancel={endPointer}
      onDoubleClick={() => setT((cur) => (cur.scale > 1 ? FIT : cur))}
    >
      <div className="flex h-full w-full items-center justify-center">
        <img
          ref={imgRef}
          src={src}
          alt={alt}
          draggable={false}
          onError={() => setFailed(true)}
          className="max-h-full max-w-full object-contain"
          style={{
            transform: `translate(${t.tx}px, ${t.ty}px) scale(${t.scale})`,
            transformOrigin: "center",
            willChange: "transform",
          }}
        />
      </div>

      {/* controls — bottom-centre, out of the way of the image */}
      <div className="absolute bottom-1 left-1/2 flex -translate-x-1/2 items-center gap-0.5 rounded border border-border bg-surface/90 px-0.5 py-0.5 text-fg-muted backdrop-blur">
        <Ctl label="Zoom out" onClick={() => zoomStep(1 / 1.4)} disabled={t.scale <= MIN_SCALE}>
          <Minus size={13} />
        </Ctl>
        <span className="w-9 text-center text-[10px] tabular-nums" title="Zoom level">
          {Math.round(t.scale * 100)}%
        </span>
        <Ctl label="Zoom in" onClick={() => zoomStep(1.4)} disabled={t.scale >= MAX_SCALE}>
          <Plus size={13} />
        </Ctl>
        <Ctl label="Fit to window" onClick={() => setT(FIT)}>
          <Maximize2 size={12} />
        </Ctl>
        <Ctl label="Actual size (1:1)" onClick={oneToOne}>
          <Scan size={12} />
        </Ctl>
      </div>
    </div>
  );
}

function Ctl({
  label,
  onClick,
  disabled,
  children,
}: {
  label: string;
  onClick: () => void;
  disabled?: boolean;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      disabled={disabled}
      onClick={onClick}
      className="flex items-center justify-center rounded p-1 hover:text-fg disabled:opacity-30 coarse:min-h-11 coarse:min-w-11"
    >
      {children}
    </button>
  );
}
