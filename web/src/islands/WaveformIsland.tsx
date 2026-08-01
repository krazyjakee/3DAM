// Lightweight audio waveform canvas. Keeping this on Canvas2D means opening audio never downloads
// or instantiates the multi-megabyte wgpu model-viewer module. Server-computed peaks remain the fast
// path; unanalyzed audio falls back to a DOM-side Web Audio decode.

import { useEffect, useMemo, useRef, useState } from "react";
import { expandSymmetricPeaks, reduceWaveform } from "@/lib/waveform";
import type { WaveformRange } from "@/lib/waveform";

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

/** Downmix an AudioBuffer to a single mono Float32Array (average of channels). */
function toMono(buf: AudioBuffer): Float32Array {
  const chs = buf.numberOfChannels;
  if (chs === 1) return buf.getChannelData(0).slice();
  const out = new Float32Array(buf.length);
  for (let c = 0; c < chs; c++) {
    const data = buf.getChannelData(c);
    for (let i = 0; i < data.length; i++) out[i] += data[i] / chs;
  }
  return out;
}

function waveformColumns(canvas: HTMLCanvasElement, maximum = 1_200) {
  return Math.max(1, Math.min(canvas.width, maximum));
}

function paint(canvas: HTMLCanvasElement, ranges: WaveformRange[], progress: number) {
  const context = canvas.getContext("2d");
  if (!context) throw new Error("Canvas2D is unavailable");

  const width = canvas.width;
  const height = canvas.height;
  const columns = ranges.length;
  const playedUntil = Math.max(0, Math.min(1, progress));
  const columnWidth = width / columns;
  const minimumHalfHeight = Math.max(1, height * 0.01);

  context.fillStyle = "#14171c";
  context.fillRect(0, 0, width, height);
  for (let column = 0; column < columns; column++) {
    const range = ranges[column];
    const top = Math.min(height / 2 - minimumHalfHeight, ((1 - range.max) * height) / 2);
    const bottom = Math.max(height / 2 + minimumHalfHeight, ((1 - range.min) * height) / 2);
    context.fillStyle = (column + 0.5) / columns <= playedUntil
      ? "#5c8cfa"
      : "#5c667a";
    context.fillRect(column * columnWidth, top, columnWidth + 0.5, bottom - top);
  }
}

export function WaveformIsland({
  src,
  progress = 0,
  peaks,
}: {
  src: string;
  progress?: number;
  peaks?: number[] | null;
}) {
  const containerRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const samplesRef = useRef<Float32Array | null>(null);
  const rangesRef = useRef<{ canvasWidth: number; ranges: WaveformRange[] } | null>(null);
  const progressRef = useRef(progress);
  const [status, setStatus] = useState<Status>("loading");
  // Prefer server-provided peaks (hosted mode, issue #73): no re-download, no re-decode. Falls back
  // to DOM decode only when the asset hasn't been analysed yet.
  const hasServerPeaks = Array.isArray(peaks) && peaks.length > 0;
  // Query data is normally referentially stable, but callers need not preserve the array identity.
  // Keying the lightweight conversion by content avoids tearing down the effect on such rerenders.
  const peaksKey = hasServerPeaks ? (peaks?.join(",") ?? "") : "";
  const serverSamples = useMemo(
    () => expandSymmetricPeaks(peaksKey.split(",").filter(Boolean).map(Number)),
    [peaksKey],
  );

  useEffect(() => {
    const container = containerRef.current;
    const canvas = canvasRef.current;
    if (!container || !canvas) return;

    let disposed = false;
    let audioCtx: AudioContext | null = null;
    // Each server peak expands to a signed pair. Never split those pairs into separate columns or a
    // sparse peak set would alternate one-sided bars when the canvas is wider than the data.
    const maximumColumns = hasServerPeaks ? Math.max(1, serverSamples.length / 2) : 1_200;

    setStatus("loading");
    samplesRef.current = null;
    rangesRef.current = null;
    fitCanvas(canvas, container);

    const ro = new ResizeObserver(() => {
      const { changed } = fitCanvas(canvas, container);
      const samples = samplesRef.current;
      if (changed && samples) {
        const cached = rangesRef.current;
        const ranges = cached?.canvasWidth === canvas.width
          ? cached.ranges
          : reduceWaveform(samples, waveformColumns(canvas, maximumColumns));
        rangesRef.current = { canvasWidth: canvas.width, ranges };
        paint(canvas, ranges, progressRef.current);
      }
    });

    (async () => {
      try {
        let samples: Float32Array;
        if (hasServerPeaks) {
          samples = serverSamples;
        } else {
          const res = await fetch(src);
          if (!res.ok) throw new Error(`content ${res.status}`);
          const bytes = await res.arrayBuffer();
          if (disposed) return;
          audioCtx = new AudioContext();
          const decoded = await audioCtx.decodeAudioData(bytes);
          samples = toMono(decoded);
        }
        if (disposed) return;
        samplesRef.current = samples;
        const ranges = reduceWaveform(samples, waveformColumns(canvas, maximumColumns));
        rangesRef.current = { canvasWidth: canvas.width, ranges };
        paint(canvas, ranges, progressRef.current);
        setStatus("ready");
      } catch (err) {
        if (!disposed) {
          console.warn("waveform island:", err);
          setStatus("error");
        }
      }
    })();

    ro.observe(container);

    return () => {
      disposed = true;
      ro.disconnect();
      audioCtx?.close().catch(() => {});
      samplesRef.current = null;
      rangesRef.current = null;
    };
  }, [src, hasServerPeaks, serverSamples]);

  // Playhead updates are cheap — a separate effect so changing `progress` doesn't rebuild the island.
  useEffect(() => {
    progressRef.current = progress;
    const canvas = canvasRef.current;
    const cached = rangesRef.current;
    if (canvas && cached?.canvasWidth === canvas.width) paint(canvas, cached.ranges, progress);
  }, [progress]);

  return (
    <div ref={containerRef} className="relative h-full w-full bg-bg">
      <canvas ref={canvasRef} className="h-full w-full" />
      {status !== "ready" && (
        <div className="pointer-events-none absolute inset-0 flex items-center justify-center text-[11px] text-fg-dim">
          {status === "loading" ? "Loading waveform…" : "Waveform unavailable."}
        </div>
      )}
    </div>
  );
}
