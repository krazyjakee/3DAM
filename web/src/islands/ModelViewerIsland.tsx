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
import { Maximize2, Minimize2, Rotate3d, Sun, Box } from "lucide-react";
import { createModelViewer } from "./index";
import type { ModelViewerHandle } from "./index";

// Match the island's default framing so the first `setCamera` doesn't jump (tech-spec 06 §5).
const DEFAULT_YAW = Math.PI / 4;
const DEFAULT_PITCH = 0.5;
const ORBIT_SPEED = 0.01;
const ZOOM_SPEED = 0.0015;
// Auto-orbit turntable speed (radians/second) — a slow, readable rotation.
const AUTO_ORBIT_SPEED = 0.5;

type Status = "loading" | "ready" | "error";

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
  const camRef = useRef({ yaw: DEFAULT_YAW, pitch: DEFAULT_PITCH, zoom: 1 });
  const [status, setStatus] = useState<Status>("loading");
  const [message, setMessage] = useState("");
  const [lighting, setLighting] = useState(0);
  const [autoOrbit, setAutoOrbit] = useState(false);
  const [wireframe, setWireframe] = useState(false);
  const [fullscreen, setFullscreen] = useState(false);

  useEffect(() => {
    const container = containerRef.current;
    const canvas = canvasRef.current;
    if (!container || !canvas) return;

    let disposed = false;
    const cam = camRef.current;
    cam.yaw = DEFAULT_YAW;
    cam.pitch = DEFAULT_PITCH;
    cam.zoom = 1;
    let dragging = false;
    let last = { x: 0, y: 0 };

    setStatus("loading");
    setMessage("");
    fitCanvas(canvas, container);

    const onPointerDown = (e: PointerEvent) => {
      dragging = true;
      last = { x: e.clientX, y: e.clientY };
      canvas.setPointerCapture(e.pointerId);
    };
    const onPointerMove = (e: PointerEvent) => {
      const handle = handleRef.current;
      if (!dragging || !handle) return;
      cam.yaw += (e.clientX - last.x) * ORBIT_SPEED;
      cam.pitch += (e.clientY - last.y) * ORBIT_SPEED;
      last = { x: e.clientX, y: e.clientY };
      handle.setCamera(cam.yaw, cam.pitch, cam.zoom);
    };
    const onPointerUp = (e: PointerEvent) => {
      dragging = false;
      if (canvas.hasPointerCapture(e.pointerId)) canvas.releasePointerCapture(e.pointerId);
    };
    const onWheel = (e: WheelEvent) => {
      const handle = handleRef.current;
      if (!handle) return;
      e.preventDefault();
      cam.zoom *= Math.exp(e.deltaY * ZOOM_SPEED);
      handle.setCamera(cam.yaw, cam.pitch, cam.zoom);
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
        const res = await fetch(src);
        if (!res.ok) throw new Error(`preview ${res.status}`);
        const bytes = new Uint8Array(await res.arrayBuffer());
        if (disposed) return;
        h.loadPreviewMesh(bytes);
        setStatus("ready");
      } catch (err) {
        if (disposed) return;
        console.error("3D preview failed:", err);
        setMessage("3D preview unavailable.");
        setStatus("error");
      }
    })();

    canvas.addEventListener("pointerdown", onPointerDown);
    canvas.addEventListener("pointermove", onPointerMove);
    canvas.addEventListener("pointerup", onPointerUp);
    canvas.addEventListener("wheel", onWheel, { passive: false });
    ro.observe(container);

    return () => {
      disposed = true;
      ro.disconnect();
      canvas.removeEventListener("pointerdown", onPointerDown);
      canvas.removeEventListener("pointermove", onPointerMove);
      canvas.removeEventListener("pointerup", onPointerUp);
      canvas.removeEventListener("wheel", onWheel);
      handleRef.current?.free();
      handleRef.current = null;
    };
  }, [src]);

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
    if (!autoOrbit || status !== "ready") return;
    let raf = 0;
    let last = performance.now();
    const tick = (t: number) => {
      const dt = Math.min(0.05, (t - last) / 1000);
      last = t;
      const cam = camRef.current;
      cam.yaw += dt * AUTO_ORBIT_SPEED;
      handleRef.current?.setCamera(cam.yaw, cam.pitch, cam.zoom);
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [autoOrbit, status]);

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
    <div ref={containerRef} className="relative h-full w-full bg-bg">
      <canvas
        ref={canvasRef}
        role="img"
        aria-label="Interactive 3D model preview — drag to orbit, scroll to zoom"
        className="h-full w-full touch-none select-none"
        style={{ cursor: status === "ready" ? "grab" : "default" }}
      />
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
            label="Auto-orbit"
            active={autoOrbit}
            onClick={() => setAutoOrbit((o) => !o)}
          >
            <Rotate3d size={14} />
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
        <div className="pointer-events-none absolute inset-0 flex items-center justify-center px-4 text-center text-[11px] text-fg-dim">
          {status === "loading" ? "Loading 3D preview…" : message}
        </div>
      )}
    </div>
  );
}

/** One control-bar button: an accent-tinted active state, a 44px touch target on coarse pointers. */
function CtrlButton({
  label,
  active,
  onClick,
  children,
}: {
  label: string;
  active: boolean;
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
      className="flex items-center justify-center rounded p-1 transition-colors coarse:min-h-11 coarse:min-w-11"
      style={{
        color: active ? "var(--color-accent)" : "var(--color-fg-muted)",
        background: active ? "var(--color-accent-muted)" : "transparent",
      }}
    >
      {children}
    </button>
  );
}
