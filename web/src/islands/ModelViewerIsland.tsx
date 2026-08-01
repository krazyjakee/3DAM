// React wrapper that mounts the 3D `ModelViewer` WASM island into the DOM layout (tech-spec 09
// §B.3). It owns the `<canvas>` and the island's lifecycle (create on mount → `free()` on unmount),
// fetches the model's server-decoded preview mesh DOM-side, and drives orbit/zoom from pointer
// events — the island just renders. WebGPU with WebGL2 fallback.
//
// The blob (`/assets/{id}/preview-mesh`) is a self-contained `DMSH` mesh the server produced with
// one Assimp decode, so *every* format (glTF/GLB, FBX, OBJ, DAE, …) previews with textures and the
// DOM never resolves external buffers — the old "missing or unreadable buffers" path is gone.
//
// On-canvas controls (issue #65): a control bar overlays lighting (studio/soft/flat), auto-orbit,
// wireframe, and fullscreen. Lighting + wireframe drive WASM methods on the handle; auto-orbit and
// fullscreen are pure DOM (a rAF that advances yaw; the Fullscreen API on the container).

import { useEffect, useRef, useState } from "react";
import { Box, Maximize2, Minimize2, Rotate3d, RotateCcw, Sun } from "lucide-react";
import { createModelViewer } from "./index";
import type { ModelViewerHandle } from "./index";
import {
  DEFAULT_VIEWER_POSE,
  keyAction,
  keyboard,
  orbit,
  pan,
  pinchPan,
  zoom,
} from "@/lib/viewer-gestures";
import type { Point, ViewerPose } from "@/lib/viewer-gestures";
// Auto-orbit turntable speed (radians/second) — a slow, readable rotation.
const AUTO_ORBIT_SPEED = 0.5;

type Status = "loading" | "ready" | "error";
type Diagnostics = {
  framing: number;
  backend: string;
  samples: number;
  sourceDraws: number;
  batchedDraws: number;
};

/** Lighting cycle for the control bar — index into this drives `handle.setLighting`. */
const LIGHTING = [
  { label: "Studio lighting" },
  { label: "Soft lighting" },
  { label: "Flat / unlit" },
];

function fitCanvas(canvas: HTMLCanvasElement, container: HTMLElement) {
  const dpr = Math.min(window.devicePixelRatio || 1, 2);
  const w = Math.max(1, Math.round(container.clientWidth * dpr));
  const h = Math.max(1, Math.round(container.clientHeight * dpr));
  const changed = canvas.width !== w || canvas.height !== h;
  if (changed) {
    canvas.width = w;
    canvas.height = h;
  }
  return { w, h, changed };
}

export function ModelViewerIsland({ src }: { src: string }) {
  const containerRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const handleRef = useRef<ModelViewerHandle | null>(null);
  // Camera lives in a ref so both the pointer handlers and the auto-orbit loop mutate one source.
  const camRef = useRef<ViewerPose>({ ...DEFAULT_VIEWER_POSE });
  const [status, setStatus] = useState<Status>("loading");
  const [message, setMessage] = useState("");
  const [retry, setRetry] = useState(0);
  const [diagnostics, setDiagnostics] = useState<Diagnostics | null>(null);
  const [lighting, setLighting] = useState(0);
  const [autoOrbit, setAutoOrbit] = useState(false);
  const [wireframe, setWireframe] = useState(false);
  const [fullscreen, setFullscreen] = useState(false);
  const [reducedMotion, setReducedMotion] = useState(false);

  useEffect(() => {
    const query = window.matchMedia("(prefers-reduced-motion: reduce)");
    const update = () => {
      setReducedMotion(query.matches);
      if (query.matches) setAutoOrbit(false);
    };
    update();
    query.addEventListener("change", update);
    return () => query.removeEventListener("change", update);
  }, []);

  useEffect(() => {
    const container = containerRef.current;
    const canvas = canvasRef.current;
    if (!container || !canvas) return;

    let disposed = false;
    const abort = new AbortController();
    camRef.current = { ...DEFAULT_VIEWER_POSE };
    const pointers = new Map<number, Point>();
    let mousePointer: number | null = null;
    let mouseMode: "orbit" | "pan" = "orbit";
    let mouseLast: Point | null = null;
    let touchPrevious: [Point, Point] | null = null;
    const touchStarts = new Map<number, Point>();
    const claimedTouches = new Set<number>();

    setStatus("loading");
    setMessage("");
    setDiagnostics(null);
    fitCanvas(canvas, container);

    const applyPose = (pose: ViewerPose) => {
      camRef.current = pose;
      handleRef.current?.setCameraPose(
        pose.yaw,
        pose.pitch,
        pose.zoom,
        pose.panX,
        pose.panY,
      );
    };

    const onPointerDown = (e: PointerEvent) => {
      setAutoOrbit(false);
      const point = { x: e.clientX, y: e.clientY };
      if (e.pointerType === "touch") {
        pointers.set(e.pointerId, point);
        touchStarts.set(e.pointerId, point);
        if (pointers.size === 2) touchPrevious = [...pointers.values()] as [Point, Point];
      } else {
        canvas.focus({ preventScroll: true });
        mousePointer = e.pointerId;
        mouseMode = e.shiftKey || e.button === 1 || e.button === 2 ? "pan" : "orbit";
        mouseLast = point;
        e.preventDefault();
      }
      canvas.setPointerCapture(e.pointerId);
    };
    const onPointerMove = (e: PointerEvent) => {
      const point = { x: e.clientX, y: e.clientY };
      if (e.pointerType === "touch") {
        const previousPoint = pointers.get(e.pointerId);
        if (!previousPoint) return;
        pointers.set(e.pointerId, point);
        if (pointers.size >= 2) {
          const current = [...pointers.values()].slice(0, 2) as [Point, Point];
          const previous = touchPrevious ?? current;
          applyPose(
            pinchPan(
              camRef.current,
              previous,
              current,
              canvas.clientWidth,
              canvas.clientHeight,
            ),
          );
          touchPrevious = current;
          e.preventDefault();
        } else {
          // `touch-action: pan-y` leaves vertical one-finger movement to the page. Horizontal moves
          // continue producing pointer events and orbit; the browser sends pointercancel when it
          // commits to page scrolling.
          const start = touchStarts.get(e.pointerId) ?? previousPoint;
          const totalX = point.x - start.x;
          const totalY = point.y - start.y;
          if (
            claimedTouches.has(e.pointerId) ||
            (Math.abs(totalX) >= 8 && Math.abs(totalX) > Math.abs(totalY) * 1.25)
          ) {
            claimedTouches.add(e.pointerId);
            applyPose(
              orbit(camRef.current, point.x - previousPoint.x, point.y - previousPoint.y),
            );
            e.preventDefault();
          }
        }
        return;
      }
      if (mousePointer !== e.pointerId || !mouseLast) return;
      const dx = point.x - mouseLast.x;
      const dy = point.y - mouseLast.y;
      applyPose(
        mouseMode === "pan"
          ? pan(camRef.current, dx, dy, canvas.clientWidth, canvas.clientHeight)
          : orbit(camRef.current, dx, dy),
      );
      mouseLast = point;
      e.preventDefault();
    };
    const finishPointer = (e: PointerEvent) => {
      pointers.delete(e.pointerId);
      touchStarts.delete(e.pointerId);
      claimedTouches.delete(e.pointerId);
      if (mousePointer === e.pointerId) {
        mousePointer = null;
        mouseLast = null;
      }
      touchPrevious = pointers.size === 2 ? ([...pointers.values()] as [Point, Point]) : null;
      if (canvas.hasPointerCapture(e.pointerId)) canvas.releasePointerCapture(e.pointerId);
    };
    const onWheel = (e: WheelEvent) => {
      // Hovering the viewer must not trap ordinary page scrolling. Click/focus it first (or use a
      // trackpad/browser pinch modifier) to make the wheel an intentional viewer zoom gesture.
      if (document.activeElement !== canvas && !e.ctrlKey && !e.metaKey) return;
      e.preventDefault();
      setAutoOrbit(false);
      applyPose(zoom(camRef.current, e.deltaY));
    };
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        canvas.blur();
        setAutoOrbit(false);
        return;
      }
      const action = keyAction(e.key, e.shiftKey);
      if (!action) return;
      e.preventDefault();
      setAutoOrbit(false);
      applyPose(keyboard(camRef.current, action));
    };
    const onContextMenu = (e: MouseEvent) => {
      if (document.activeElement === canvas) e.preventDefault();
    };

    const ro = new ResizeObserver(() => {
      const { w, h, changed } = fitCanvas(canvas, container);
      if (changed && handleRef.current) handleRef.current.resize(w, h);
    });

    (async () => {
      try {
        const h = await createModelViewer(canvas);
        if (disposed) {
          h.free();
          return;
        }
        handleRef.current = h;
        const res = await fetch(src, { signal: abort.signal });
        if (!res.ok) throw new Error(`preview ${res.status}`);
        const bytes = new Uint8Array(await res.arrayBuffer());
        if (disposed) return;
        h.loadPreviewMesh(bytes);
        setDiagnostics({
          framing: h.framingVersion,
          backend: h.backend,
          samples: h.antialiasingSamples,
          sourceDraws: h.sourceDrawCount,
          batchedDraws: h.batchedDrawCount,
        });
        setStatus("ready");
      } catch (err) {
        if (disposed) return;
        console.error("3D preview failed:", err);
        setMessage(
          "3D preview unavailable. Your thumbnail and asset details are still available.",
        );
        setStatus("error");
      }
    })();

    canvas.addEventListener("pointerdown", onPointerDown);
    canvas.addEventListener("pointermove", onPointerMove);
    canvas.addEventListener("pointerup", finishPointer);
    canvas.addEventListener("pointercancel", finishPointer);
    canvas.addEventListener("wheel", onWheel, { passive: false });
    canvas.addEventListener("keydown", onKeyDown);
    canvas.addEventListener("contextmenu", onContextMenu);
    ro.observe(container);

    return () => {
      disposed = true;
      abort.abort();
      ro.disconnect();
      canvas.removeEventListener("pointerdown", onPointerDown);
      canvas.removeEventListener("pointermove", onPointerMove);
      canvas.removeEventListener("pointerup", finishPointer);
      canvas.removeEventListener("pointercancel", finishPointer);
      canvas.removeEventListener("wheel", onWheel);
      canvas.removeEventListener("keydown", onKeyDown);
      canvas.removeEventListener("contextmenu", onContextMenu);
      handleRef.current?.free();
      handleRef.current = null;
    };
  }, [src, retry]);

  // Push lighting / wireframe to the island whenever they change — and once more when the model
  // becomes ready (a fresh handle starts at studio/solid, so re-apply the user's current choice).
  useEffect(() => {
    if (status === "ready") handleRef.current?.setLighting(lighting);
  }, [lighting, status]);
  useEffect(() => {
    if (status === "ready") handleRef.current?.setWireframe(wireframe);
  }, [wireframe, status]);

  // Auto-orbit: a rAF turntable that advances yaw and drives the shared camera. Stops on toggle-off
  // and unmount. Manual drag still composes — both mutate `camRef`.
  useEffect(() => {
    if (!autoOrbit || reducedMotion || status !== "ready") return;
    let raf = 0;
    let last = performance.now();
    const tick = (t: number) => {
      const dt = Math.min(0.05, (t - last) / 1000);
      last = t;
      const cam = { ...camRef.current, yaw: camRef.current.yaw + dt * AUTO_ORBIT_SPEED };
      camRef.current = cam;
      handleRef.current?.setCameraPose(cam.yaw, cam.pitch, cam.zoom, cam.panX, cam.panY);
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [autoOrbit, reducedMotion, status]);

  // Keep the fullscreen toggle in sync with the actual state (Esc / OS chrome can exit it).
  useEffect(() => {
    const onChange = () => setFullscreen(document.fullscreenElement === containerRef.current);
    document.addEventListener("fullscreenchange", onChange);
    return () => document.removeEventListener("fullscreenchange", onChange);
  }, []);
  const toggleFullscreen = () => {
    const el = containerRef.current;
    if (!document.fullscreenElement) el?.requestFullscreen?.();
    else document.exitFullscreen?.();
  };

  return (
    <div
      ref={containerRef}
      className="relative h-full w-full bg-bg"
      data-viewer-backend={diagnostics?.backend}
      data-viewer-framing={diagnostics?.framing}
      data-viewer-msaa={diagnostics?.samples}
      data-viewer-source-draws={diagnostics?.sourceDraws}
      data-viewer-batched-draws={diagnostics?.batchedDraws}
    >
      <canvas
        ref={canvasRef}
        role="region"
        tabIndex={0}
        aria-label="Interactive 3D model preview"
        aria-describedby="model-viewer-instructions"
        className="h-full w-full select-none outline-none focus-visible:ring-2 focus-visible:ring-accent"
        style={{
          cursor: status === "ready" ? "grab" : "default",
          // Preserve one-finger vertical page scrolling; horizontal single-finger orbit and custom
          // two-pointer pan/pinch remain available to the viewer.
          touchAction: "pan-y",
        }}
      />
      <span id="model-viewer-instructions" className="sr-only">
        Drag to orbit. Shift-drag, middle-drag, right-drag, or two-finger drag to pan. Pinch to
        zoom. Click the preview before using the wheel. Arrow keys orbit, Shift plus arrow keys pan,
        plus and minus zoom, Home resets, and Escape returns wheel scrolling to the page.
      </span>
      {status === "ready" && (
        <div className="absolute top-1.5 right-1.5 flex items-center gap-1 rounded border border-border bg-surface/85 p-0.5 backdrop-blur">
          <CtrlButton
            label={`${LIGHTING[lighting].label} — click to cycle`}
            active={lighting !== 0}
            onClick={() => setLighting((m) => (m + 1) % LIGHTING.length)}
          >
            <Sun size={14} />
          </CtrlButton>
          <CtrlButton
            label="Wireframe"
            active={wireframe}
            onClick={() => setWireframe((w) => !w)}
          >
            <Box size={14} />
          </CtrlButton>
          <CtrlButton
            label={
              reducedMotion
                ? "Auto-orbit unavailable while reduced motion is enabled"
                : "Auto-orbit"
            }
            active={autoOrbit}
            disabled={reducedMotion}
            onClick={() => setAutoOrbit((o) => !o)}
          >
            <Rotate3d size={14} />
          </CtrlButton>
          <CtrlButton
            label="Reset camera"
            onClick={() => {
              camRef.current = { ...DEFAULT_VIEWER_POSE };
              const pose = camRef.current;
              handleRef.current?.setCameraPose(
                pose.yaw,
                pose.pitch,
                pose.zoom,
                pose.panX,
                pose.panY,
              );
              setAutoOrbit(false);
            }}
          >
            <RotateCcw size={14} />
          </CtrlButton>
          <CtrlButton
            label={fullscreen ? "Exit fullscreen" : "Fullscreen"}
            active={fullscreen}
            onClick={toggleFullscreen}
          >
            {fullscreen ? <Minimize2 size={14} /> : <Maximize2 size={14} />}
          </CtrlButton>
        </div>
      )}
      {status !== "ready" && (
        <div
          className="absolute inset-0 flex flex-col items-center justify-center gap-2 px-4 text-center text-[11px] text-fg-dim"
          role={status === "error" ? "alert" : "status"}
          aria-live="polite"
        >
          <span>{status === "loading" ? "Loading 3D preview…" : message}</span>
          {status === "error" && (
            <button type="button" className="btn" onClick={() => setRetry((value) => value + 1)}>
              Retry 3D preview
            </button>
          )}
        </div>
      )}
    </div>
  );
}

/** One control-bar button: an accent-tinted active state, a 44px touch target on coarse pointers. */
function CtrlButton({
  label,
  active,
  disabled = false,
  onClick,
  children,
}: {
  label: string;
  active?: boolean;
  disabled?: boolean;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={label}
      aria-label={label}
      aria-pressed={active}
      disabled={disabled}
      className="flex items-center justify-center rounded p-1 transition-colors motion-reduce:transition-none coarse:min-h-11 coarse:min-w-11 disabled:cursor-not-allowed disabled:opacity-40"
      style={{
        color: active ? "var(--color-accent)" : "var(--color-fg-muted)",
        background: active ? "var(--color-accent-muted)" : "transparent",
      }}
    >
      {children}
    </button>
  );
}
