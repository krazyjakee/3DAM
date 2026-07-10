// Inline audio player for the Inspector (issues #16, #14). An <audio> element streams the asset's
// content bytes; play/pause + a seek slider drive it, and the current position feeds the existing
// WaveformIsland's `progress` prop so the playhead actually moves (it was frozen at 0 because the
// call site passed no progress). Clicking the waveform seeks too. Decode/stream failures degrade to
// a clear message rather than a dead control (fail-soft, DESIGN_GUIDELINES §2).

import { useEffect, useRef, useState } from "react";
import { Pause, Play } from "lucide-react";
import { WaveformIsland } from "@/islands/WaveformIsland";
import { clearAutoplay, useAutoplaySignal } from "@/lib/audio-intent";

/** m:ss, guarding the NaN/Infinity that HTMLMediaElement reports before metadata loads. */
function fmtTime(sec: number): string {
  if (!Number.isFinite(sec) || sec < 0) return "0:00";
  const m = Math.floor(sec / 60);
  const s = Math.floor(sec % 60);
  return `${m}:${String(s).padStart(2, "0")}`;
}

export function AudioPlayer({
  src,
  assetId,
  peaks,
}: {
  src: string;
  assetId?: string;
  peaks?: number[] | null;
}) {
  const audioRef = useRef<HTMLAudioElement>(null);
  const [playing, setPlaying] = useState(false);
  const [time, setTime] = useState(0);
  const [dur, setDur] = useState(0);
  const [error, setError] = useState(false);

  const progress = dur > 0 ? time / dur : 0;

  // Double-click-to-play (issue #52): when the Browser requests autoplay for this asset, start
  // playback and clear the request so single-click selecting it later never auto-starts.
  const autoplaySignal = useAutoplaySignal(assetId);
  useEffect(() => {
    if (autoplaySignal > 0) {
      audioRef.current?.play().catch(() => setError(true));
      clearAutoplay();
    }
  }, [autoplaySignal]);

  const toggle = () => {
    const a = audioRef.current;
    if (!a) return;
    if (a.paused) a.play().catch(() => setError(true));
    else a.pause();
  };

  const seekToFraction = (f: number) => {
    const a = audioRef.current;
    if (!a || !Number.isFinite(a.duration)) return;
    a.currentTime = Math.max(0, Math.min(1, f)) * a.duration;
    setTime(a.currentTime);
  };

  return (
    <div className="flex flex-col">
      {/* waveform doubles as a click-to-seek scrubber */}
      <div
        className="relative h-16 cursor-pointer"
        onClick={(e) => {
          const r = e.currentTarget.getBoundingClientRect();
          seekToFraction((e.clientX - r.left) / r.width);
        }}
      >
        <WaveformIsland src={src} progress={progress} peaks={peaks} />
      </div>

      <div className="flex items-center gap-2 px-2 py-1.5">
        <button
          type="button"
          onClick={toggle}
          disabled={error}
          aria-label={playing ? "Pause" : "Play"}
          title={playing ? "Pause" : "Play"}
          className="flex items-center justify-center rounded border border-border bg-surface-2 p-1.5 text-fg-muted hover:text-fg disabled:opacity-40 coarse:min-h-11 coarse:min-w-11"
        >
          {playing ? <Pause size={13} /> : <Play size={13} />}
        </button>
        <input
          type="range"
          min={0}
          max={1}
          step={0.001}
          value={progress}
          disabled={error || dur === 0}
          onChange={(e) => seekToFraction(Number(e.target.value))}
          aria-label="Seek"
          className="h-1 flex-1 cursor-pointer coarse:h-2"
          style={{ accentColor: "var(--color-accent)" }}
        />
        <span className="shrink-0 text-[10px] text-fg-dim tabular-nums">
          {fmtTime(time)} / {fmtTime(dur)}
        </span>
      </div>

      {error && (
        <p className="px-2 pb-1.5 text-[10px] text-danger">
          Can’t play this audio format in the browser.
        </p>
      )}

      <audio
        ref={audioRef}
        src={src}
        preload="metadata"
        onLoadedMetadata={(e) => setDur(e.currentTarget.duration)}
        onDurationChange={(e) => setDur(e.currentTarget.duration)}
        onTimeUpdate={(e) => setTime(e.currentTarget.currentTime)}
        onPlay={() => setPlaying(true)}
        onPause={() => setPlaying(false)}
        onEnded={() => setPlaying(false)}
        onError={() => setError(true)}
      />
    </div>
  );
}
