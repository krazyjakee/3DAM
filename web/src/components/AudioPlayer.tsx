// Inline audio player for the Inspector (issues #16, #14). An <audio> element streams the asset's
// content bytes; play/pause + a seek slider drive it, and the current position feeds the existing
// WaveformIsland's `progress` prop so the playhead actually moves (it was frozen at 0 because the
// call site passed no progress). Clicking the waveform seeks too. Decode/stream failures degrade to
// a clear message rather than a dead control (fail-soft, DESIGN_GUIDELINES §2).

import { useEffect, useRef, useState } from "react";
import { Pause, Play } from "lucide-react";
import { WaveformIsland } from "@/islands/WaveformIsland";
import { clearAutoplay, useAutoplaySignal } from "@/lib/audio-intent";
import { shortcutLabel, SHORTCUT_EVENT, type ShortcutId } from "@/lib/shortcuts";

/** `MediaError` codes, spelled out: jsdom ships no `MediaError` global, so reading the constants off
 *  it would work in both shells and throw in the component test. */
const MEDIA_ERR_NETWORK = 2;
const MEDIA_ERR_DECODE = 3;
const MEDIA_ERR_SRC_NOT_SUPPORTED = 4;

/** What actually went wrong, read off the element instead of guessed.
 *
 *  This used to read "can't play this audio format in the browser" for every failure, which was
 *  wrong twice over. The desktop shell is a webview, not a browser — same UI, so the copy cannot
 *  name one of them. And the format is rarely the cause: a content request that 404s (an expired
 *  media ticket, or a federated peer that can't serve the bytes) raises the same `error` event as a
 *  codec nothing here can decode. `MediaError` separates the transfer from the decode, so say only
 *  what the code actually supports; code 4 means "never became a playable stream" and genuinely
 *  does not distinguish the two, so it names both. */
function playbackFailure(el: HTMLAudioElement | null): string {
  switch (el?.error?.code) {
    case MEDIA_ERR_NETWORK:
      return "Lost the connection while loading this audio.";
    case MEDIA_ERR_DECODE:
      return "This audio stopped decoding partway — the file may be damaged.";
    case MEDIA_ERR_SRC_NOT_SUPPORTED:
      return "Couldn’t play this audio — 3DAM couldn’t fetch the file, or nothing here can decode it.";
    default:
      return "Couldn’t play this audio.";
  }
}

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
  onCredentialExpired,
}: {
  src: string;
  assetId?: string;
  peaks?: number[] | null;
  onCredentialExpired?: () => void;
}) {
  const audioRef = useRef<HTMLAudioElement>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const [playing, setPlaying] = useState(false);
  const [time, setTime] = useState(0);
  const [dur, setDur] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const lastRenewal = useRef(0);

  const progress = dur > 0 ? time / dur : 0;

  // A `play()` promise rejects with `AbortError` whenever a load or a pause interrupts it, and this
  // component swaps `src` under a live element on every ticket renewal — routine, and not something
  // to report as a failure. Anything else did stop playback, so ask the element why.
  const reportPlayFailure = (reason: unknown) => {
    if ((reason as { name?: string } | null)?.name === "AbortError") return;
    setError(playbackFailure(audioRef.current));
  };

  // The `lg` rail and the responsive drawer are both mounted, so a selected audio asset always has
  // *two* players in the DOM — and a `display:none` <audio> still makes sound. Every path that
  // starts playback must therefore address only the player the user can see, or the file plays
  // twice, a beat apart, and the audible copy can't be paused. `display:none` reports no client
  // rects; the closed (translated off-screen) drawer panel does, which is the same instance the
  // play/pause shortcut already drives.
  const visible = () => (rootRef.current?.getClientRects().length ?? 0) > 0;

  // Crossing the `lg` breakpoint mid-playback hides one copy and reveals the other *without*
  // unmounting either (they differ by a `hidden`/`lg:hidden` class, not by being rendered), so a
  // playing player can become the one nothing controls. Hand playback back with the visibility.
  useEffect(() => {
    const onResize = () => {
      const a = audioRef.current;
      if (a && !a.paused && !visible()) a.pause();
    };
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);

  // A media-ticket renewal swaps `src` under a live element (`useMediaTicket`). The load algorithm
  // then aborts playback, fires `pause`, and reports currentTime 0 — so the position and the intent
  // to be playing must be captured in the render that introduces the new URL, before any of that
  // reaches state. Restored once the replacement source has metadata.
  const shownSrc = useRef(src);
  const resume = useRef<{ at: number; playing: boolean } | null>(null);
  if (shownSrc.current !== src) {
    shownSrc.current = src;
    resume.current = { at: time, playing };
  }

  // Double-click-to-play (issue #52): when the Browser requests autoplay for this asset, start
  // playback and clear the request so single-click selecting it later never auto-starts.
  const autoplaySignal = useAutoplaySignal(assetId);
  useEffect(() => {
    if (autoplaySignal > 0 && visible()) {
      audioRef.current?.play().catch(reportPlayFailure);
      clearAutoplay();
    }
  }, [autoplaySignal]);

  const toggle = () => {
    const a = audioRef.current;
    if (!a) return;
    if (a.paused) a.play().catch(reportPlayFailure);
    else a.pause();
  };

  useEffect(() => {
    const onShortcut = (event: Event) => {
      if ((event as CustomEvent<ShortcutId>).detail !== "play-pause") return;
      if (visible()) toggle();
    };
    window.addEventListener(SHORTCUT_EVENT, onShortcut);
    return () => window.removeEventListener(SHORTCUT_EVENT, onShortcut);
  });

  const seekToFraction = (f: number) => {
    const a = audioRef.current;
    if (!a || !Number.isFinite(a.duration)) return;
    a.currentTime = Math.max(0, Math.min(1, f)) * a.duration;
    setTime(a.currentTime);
  };

  return (
    <div ref={rootRef} className="flex flex-col">
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
          disabled={error !== null}
          aria-label={playing ? "Pause" : "Play"}
          aria-keyshortcuts="Space"
          title={`${playing ? "Pause" : "Play"} (${shortcutLabel("play-pause")})`}
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
          disabled={error !== null || dur === 0}
          onChange={(e) => seekToFraction(Number(e.target.value))}
          aria-label="Seek"
          className="h-1 flex-1 cursor-pointer coarse:h-2"
          style={{ accentColor: "var(--color-accent)" }}
        />
        <span className="shrink-0 text-[10px] text-fg-dim tabular-nums">
          {fmtTime(time)} / {fmtTime(dur)}
        </span>
      </div>

      {error !== null && <p className="px-2 pb-1.5 text-[10px] text-danger">{error}</p>}

      <audio
        ref={audioRef}
        src={src}
        preload="metadata"
        onLoadedMetadata={(e) => {
          const el = e.currentTarget;
          setDur(el.duration);
          setError(null);
          const restore = resume.current;
          resume.current = null;
          if (!restore) return;
          if (restore.at > 0) el.currentTime = restore.at;
          // The interruption was a credential rotation the listener never asked for; carry on.
          if (restore.playing) el.play().catch(reportPlayFailure);
        }}
        onDurationChange={(e) => setDur(e.currentTarget.duration)}
        onTimeUpdate={(e) => setTime(e.currentTarget.currentTime)}
        onPlay={() => setPlaying(true)}
        onPause={() => setPlaying(false)}
        onEnded={() => setPlaying(false)}
        onError={(e) => {
          if (onCredentialExpired && Date.now() - lastRenewal.current >= 60_000) {
            lastRenewal.current = Date.now();
            onCredentialExpired();
          } else {
            setError(playbackFailure(e.currentTarget));
          }
        }}
      />
    </div>
  );
}
