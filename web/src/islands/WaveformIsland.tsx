// React wrapper that mounts the audio `WaveformView` WASM island (tech-spec 09 §B.3, ADR 0009 §9 —
// waveforms are a WASM island; thumbnails stay server-rendered). The DOM fetches the audio bytes and
// decodes them to mono samples with the Web Audio API, then hands the samples across the boundary;
// the island reduces them to peaks and renders. `progress` (0..1) drives the playhead.

import { useEffect, useRef, useState } from "react";
import { createWaveform } from "./index";
import type { WaveformHandle } from "./index";

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

/** Expand server-side peaks (0–1 per bucket) into interleaved ±amplitude "samples" the island
 *  reduces to symmetric bars — so a pre-computed waveform draws with no client-side decode (#73). */
function peaksToSamples(peaks: number[]): Float32Array {
  const out = new Float32Array(peaks.length * 2);
  for (let i = 0; i < peaks.length; i++) {
    const p = peaks[i];
    out[i * 2] = -p;
    out[i * 2 + 1] = p;
  }
  return out;
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
  const handleRef = useRef<WaveformHandle | null>(null);
  const [status, setStatus] = useState<Status>("loading");
  // Prefer server-provided peaks (hosted mode, issue #73): no re-download, no re-decode. Falls back
  // to DOM decode only when the asset hasn't been analysed yet.
  const hasServerPeaks = Array.isArray(peaks) && peaks.length > 0;

  useEffect(() => {
    const container = containerRef.current;
    const canvas = canvasRef.current;
    if (!container || !canvas) return;

    let disposed = false;
    let audioCtx: AudioContext | null = null;

    setStatus("loading");
    fitCanvas(canvas, container);

    const ro = new ResizeObserver(() => {
      const { w, h, changed } = fitCanvas(canvas, container);
      if (changed && handleRef.current) handleRef.current.resize(w, h);
    });

    (async () => {
      try {
        const h = await createWaveform(canvas);
        if (disposed) {
          h.free();
          return;
        }
        handleRef.current = h;
        if (hasServerPeaks) {
          // Draw straight from the server array — the whole point of #73.
          h.setWaveform(peaksToSamples(peaks as number[]));
          h.setProgress(progress);
          setStatus("ready");
          return;
        }
        const res = await fetch(src);
        if (!res.ok) throw new Error(`content ${res.status}`);
        const bytes = await res.arrayBuffer();
        if (disposed) return;
        audioCtx = new AudioContext();
        const decoded = await audioCtx.decodeAudioData(bytes);
        if (disposed) return;
        h.setWaveform(toMono(decoded));
        h.setProgress(progress);
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
      handleRef.current?.free();
      handleRef.current = null;
    };
  }, [src, hasServerPeaks]);

  // Playhead updates are cheap — a separate effect so changing `progress` doesn't rebuild the island.
  useEffect(() => {
    handleRef.current?.setProgress(progress);
  }, [progress]);

  return (
    <div ref={containerRef} className="relative h-full w-full bg-bg">
      <canvas ref={canvasRef} className="h-full w-full" />
      {status !== "ready" && (
        <div className="pointer-events-none absolute inset-0 flex items-center justify-center text-[11px] text-fg-dim">
          {status === "loading" ? "Decoding waveform…" : "Waveform unavailable."}
        </div>
      )}
    </div>
  );
}
