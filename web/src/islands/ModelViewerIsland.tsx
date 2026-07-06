// React wrapper that mounts the 3D `ModelViewer` WASM island into the DOM layout (tech-spec 09
// §B.3). It owns the `<canvas>` and the island's lifecycle (create on mount → `free()` on unmount),
// fetches the model bytes DOM-side, and drives orbit/zoom from pointer events — the island just
// renders. WebGPU with WebGL2 fallback; a decode failure (e.g. a loose `.gltf` with external
// buffers) degrades to a readable message instead of a blank canvas.

import { useEffect, useRef, useState } from "react";
import { createModelViewer } from "./index";
import type { ModelViewerHandle } from "./index";

// Match the island's default framing so the first `setCamera` doesn't jump (tech-spec 06 §5).
const DEFAULT_YAW = Math.PI / 4;
const DEFAULT_PITCH = 0.5;
const ORBIT_SPEED = 0.01;
const ZOOM_SPEED = 0.0015;

type Status = "loading" | "ready" | "error";

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
  const [status, setStatus] = useState<Status>("loading");
  const [message, setMessage] = useState("");

  useEffect(() => {
    const container = containerRef.current;
    const canvas = canvasRef.current;
    if (!container || !canvas) return;

    let disposed = false;
    let handle: ModelViewerHandle | null = null;
    const cam = { yaw: DEFAULT_YAW, pitch: DEFAULT_PITCH, zoom: 1 };
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
      if (!handle) return;
      e.preventDefault();
      cam.zoom *= Math.exp(e.deltaY * ZOOM_SPEED);
      handle.setCamera(cam.yaw, cam.pitch, cam.zoom);
    };

    const ro = new ResizeObserver(() => {
      const { w, h, changed } = fitCanvas(canvas, container);
      if (changed && handle) handle.resize(w, h);
    });

    (async () => {
      try {
        const h = await createModelViewer(canvas);
        if (disposed) {
          h.free();
          return;
        }
        handle = h;
        const res = await fetch(src);
        if (!res.ok) throw new Error(`content ${res.status}`);
        const buf = await res.arrayBuffer();
        if (disposed) return;
        handle.loadModel(new Uint8Array(buf));
        setStatus("ready");
      } catch (err) {
        if (disposed) return;
        const msg = String(err);
        // Loose glTF (external .bin/textures) can't be resolved from bytes alone yet.
        setMessage(
          msg.includes("glTF decode")
            ? "Preview needs a self-contained .glb (loose glTF isn't supported yet)."
            : "3D preview unavailable in this browser.",
        );
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
      handle?.free();
    };
  }, [src]);

  return (
    <div ref={containerRef} className="relative h-full w-full bg-bg">
      <canvas
        ref={canvasRef}
        className="h-full w-full touch-none select-none"
        style={{ cursor: status === "ready" ? "grab" : "default" }}
      />
      {status !== "ready" && (
        <div className="pointer-events-none absolute inset-0 flex items-center justify-center px-4 text-center text-[11px] text-fg-dim">
          {status === "loading" ? "Loading 3D preview…" : message}
        </div>
      )}
    </div>
  );
}
