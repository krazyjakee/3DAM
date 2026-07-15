import { useEffect, useRef, useState } from "react";
import { ChevronUp, Loader2, LogIn, Server, X } from "lucide-react";
import { useCancelJob, useJobs, useStats, useVersion } from "@/api/queries";
import { useConnection, type ConnState } from "@/api/connection";
import type { JobStatus, MediaType } from "@/api/types";
import { getServer, isRemote, serverLabel } from "@/lib/server";
import { ConnectDialog } from "./ConnectDialog";
import { TokenLoginForm } from "./AuthGate";

/** Bottom status strip: active scan/analysis jobs with live progress, the live-connection state, and
 *  the server build. A single job shows inline; concurrent jobs condense into one aggregate bar with
 *  an overall percentage + ETA that expands to the individual jobs (issue #59). The connection dot
 *  makes a silent disconnect visible (issues #24, #25). */
export function StatusBar() {
  const version = useVersion();
  const jobs = useJobs({});
  const cancel = useCancelJob();
  const conn = useConnection();

  const active = (jobs.data?.items ?? []).filter(
    (j) => j.state === "running" || j.state === "queued",
  );

  return (
    <footer className="flex h-7 shrink-0 items-center gap-3 border-t border-border bg-surface px-3 text-[11px] text-fg-dim">
      <JobAnnouncer active={active} />
      {active.length === 0 ? (
        <span className="flex-1">Idle</span>
      ) : active.length === 1 ? (
        <div className="flex min-w-0 flex-1 items-center">
          <JobPill job={active[0]} onCancel={() => cancel.mutate(active[0].id)} />
        </div>
      ) : (
        <AggregateJobs jobs={active} onCancel={(id) => cancel.mutate(id)} />
      )}
      <MediaBreakdown />
      <ConnectionPill state={conn.state} />
      {version.data?.auth === "anonymous" && <SignedOutChip />}
      <ServerChip />
      <span className="tabular-nums">{version.data?.server ?? ""}</span>
    </footer>
  );
}

/** Anonymous-mode posture chip (front-door auth): browsing is public but writes need a credential,
 *  so a signed-out session shows exactly one affordance — Sign in — instead of failing writes with
 *  raw errors. Signed in, the chip disappears (the token rides every request; scopes decide). */
function SignedOutChip() {
  const [open, setOpen] = useState(false);
  if (getServer().token) return null;
  return (
    <>
      <button
        className="flex items-center gap-1 rounded px-1.5 py-0.5 text-warn hover:text-fg"
        onClick={() => setOpen(true)}
        title="Browsing read-only — sign in to make changes"
      >
        <LogIn size={11} />
        <span>Signed out · Sign in</span>
      </button>
      {open && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
          onClick={() => setOpen(false)}
        >
          <div
            className="w-full max-w-md rounded-lg border border-border bg-surface p-4 shadow-xl"
            onClick={(e) => e.stopPropagation()}
          >
            <TokenLoginForm reason={null} allowReadOnly={false} onClose={() => setOpen(false)} />
          </div>
        </div>
      )}
    </>
  );
}

/** Connection target chip (hosted mode, issue #74): shows "Local" or the remote host, and opens the
 *  Connect dialog to point the client at a different 3DAM server. Mirrors the native GUI chip (#70). */
function ServerChip() {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button
        className={`flex items-center gap-1 rounded px-1.5 py-0.5 hover:text-fg ${
          isRemote() ? "text-accent" : "text-fg-dim"
        }`}
        onClick={() => setOpen(true)}
        title="Connect to a 3DAM server"
      >
        <Server size={11} />
        <span className="max-w-[160px] truncate">{serverLabel()}</span>
      </button>
      {open && <ConnectDialog onClose={() => setOpen(false)} />}
    </>
  );
}

/** Per-media asset totals on the right of the bar — `3D 412 · IMG 590 · SFX 246` (issue #64,
 *  DESIGN_GUIDELINES footer). Same hue + short label per media type as the grid/table `MediaBadge`
 *  (model → 3D indigo, image → IMG orange, audio → SFX teal); reads the already-cached `by_media`
 *  stats. Media types with no assets are omitted so an audio-free library shows just `3D · IMG`. */
const MEDIA_BREAKDOWN: { key: MediaType; label: string; full: string; color: string }[] = [
  { key: "model", label: "3D", full: "3D models", color: "var(--color-media-model)" },
  { key: "image", label: "IMG", full: "images", color: "var(--color-media-image)" },
  { key: "audio", label: "SFX", full: "audio", color: "var(--color-media-audio)" },
];

function MediaBreakdown() {
  const stats = useStats();
  const byMedia = stats.data?.by_media ?? {};
  const shown = MEDIA_BREAKDOWN.map((m) => ({ ...m, count: byMedia[m.key] ?? 0 })).filter(
    (m) => m.count > 0,
  );
  if (shown.length === 0) return null;
  return (
    <span
      className="hidden items-center gap-2 sm:flex"
      aria-label={`Assets by media type: ${shown.map((m) => `${m.count} ${m.full}`).join(", ")}`}
    >
      {shown.map((m, i) => (
        <span key={m.key} className="flex items-center gap-1.5">
          {i > 0 && <span className="text-fg-dim/60">·</span>}
          <span style={{ color: m.color }}>{m.label}</span>
          <span className="tabular-nums text-fg">{m.count.toLocaleString()}</span>
        </span>
      ))}
    </span>
  );
}

/** Screen-reader announcements for background jobs (a11y hardening, issue #44). A polite, visually
 *  hidden live region that speaks only coarse lifecycle transitions — jobs starting, or all jobs
 *  finishing — rather than every progress tick, so assistive tech isn't spammed with percentages
 *  (the visual JobPill still shows the live %). */
function JobAnnouncer({ active }: { active: JobStatus[] }) {
  const [msg, setMsg] = useState("");
  const prev = useRef(0);
  const count = active.length;
  useEffect(() => {
    if (count > prev.current) {
      const kinds = [...new Set(active.map((j) => j.kind))].join(", ");
      setMsg(`${count} background ${count === 1 ? "task" : "tasks"} running: ${kinds}`);
    } else if (count === 0 && prev.current > 0) {
      setMsg("Background tasks complete");
    }
    prev.current = count;
    // Keyed on the active-job count only: announce start/finish, not each progress update.
  }, [count]);
  return (
    <span className="sr-only" role="status" aria-live="polite" aria-atomic="true">
      {msg}
    </span>
  );
}

const CONN_META: Record<ConnState, { color: string; label: string | null; pulse: boolean }> = {
  online: { color: "var(--color-lic-permissive)", label: null, pulse: false },
  reconnecting: { color: "var(--color-warn)", label: "Reconnecting…", pulse: true },
  offline: { color: "var(--color-danger)", label: "Offline", pulse: false },
};

function ConnectionPill({ state }: { state: ConnState }) {
  const { color, label, pulse } = CONN_META[state];
  return (
    <span
      className="flex items-center gap-1.5"
      title={
        state === "online"
          ? "Live updates connected"
          : state === "reconnecting"
            ? "Live updates paused — reconnecting"
            : "Can't reach the server"
      }
    >
      <span
        className={`h-1.5 w-1.5 shrink-0 rounded-full ${pulse ? "animate-pulse" : ""}`}
        style={{ background: color }}
      />
      {label && <span style={{ color }}>{label}</span>}
    </span>
  );
}

/** The single-job readout: kind, a progress bar, %, ETA, the current item, and a cancel button. */
function JobPill({ job, onCancel }: { job: JobStatus; onCancel: () => void }) {
  const { done, total, current } = job.progress;
  const pct = total ? Math.min(100, Math.round((done / total) * 100)) : null;
  const eta = useEta(job.id, done, total ?? null);
  return (
    <div className="flex min-w-0 items-center gap-2">
      <Loader2 size={12} className="animate-spin text-accent" />
      <span className="capitalize">{job.kind}</span>
      <ProgressBar pct={pct} />
      <span className="tabular-nums">{pct != null ? `${pct}%` : done.toLocaleString()}</span>
      {eta && <span className="tabular-nums text-fg-dim">~{eta} left</span>}
      {current && (
        <span className="hidden max-w-[220px] truncate text-fg-dim md:inline" title={current}>
          {current}
        </span>
      )}
      <button className="text-fg-dim hover:text-danger" onClick={onCancel} title="Cancel">
        <X size={12} />
      </button>
    </div>
  );
}

/** Concurrent jobs condensed into one bar: aggregate done/total across all active jobs, an overall %
 *  and ETA, expandable to the individual jobs (each cancellable). */
function AggregateJobs({ jobs, onCancel }: { jobs: JobStatus[]; onCancel: (id: string) => void }) {
  const [open, setOpen] = useState(false);
  const done = jobs.reduce((s, j) => s + j.progress.done, 0);
  const totals = jobs.map((j) => j.progress.total);
  // Aggregate % + ETA only when every active job has a known total; otherwise it's indeterminate.
  const total = totals.every((t): t is number => t != null && t > 0)
    ? totals.reduce((s: number, t) => s + t, 0)
    : null;
  const pct = total ? Math.min(100, Math.round((done / total) * 100)) : null;
  const eta = useEta("aggregate", done, total);

  const kinds = [...new Set(jobs.map((j) => j.kind))];
  const label = kinds.length === 1 ? `${jobs.length} ${kinds[0]}s` : `${jobs.length} jobs`;

  return (
    <div className="relative flex min-w-0 flex-1 items-center gap-2">
      <Loader2 size={12} className="shrink-0 animate-spin text-accent" />
      <button
        className="flex items-center gap-1 capitalize hover:text-fg"
        onClick={() => setOpen((o) => !o)}
        title="Show individual jobs"
        aria-expanded={open}
      >
        {label}
        <ChevronUp
          size={11}
          className="transition-transform"
          style={{ transform: open ? "none" : "rotate(180deg)" }}
        />
      </button>
      <ProgressBar pct={pct} />
      <span className="tabular-nums">
        {pct != null ? `${pct}%` : `${done.toLocaleString()} done`}
      </span>
      {eta && <span className="tabular-nums text-fg-dim">~{eta} left</span>}

      {open && (
        <div className="absolute bottom-full left-0 mb-1.5 flex max-h-64 w-[380px] max-w-[80vw] flex-col gap-1 overflow-y-auto rounded-md border border-border bg-surface p-1.5 shadow-xl">
          {jobs.map((j) => {
            const p = j.progress.total
              ? Math.min(100, Math.round((j.progress.done / j.progress.total) * 100))
              : null;
            return (
              <div
                key={j.id}
                className="flex items-center gap-2 rounded px-1.5 py-1 hover:bg-surface-2"
              >
                <span className="w-16 shrink-0 capitalize text-fg-muted">{j.kind}</span>
                <ProgressBar pct={p} />
                <span className="w-9 shrink-0 text-right tabular-nums">
                  {p != null ? `${p}%` : j.progress.done.toLocaleString()}
                </span>
                {j.progress.current && (
                  <span className="min-w-0 flex-1 truncate text-fg-dim" title={j.progress.current}>
                    {j.progress.current}
                  </span>
                )}
                <button
                  className="shrink-0 text-fg-dim hover:text-danger"
                  onClick={() => onCancel(j.id)}
                  title="Cancel this job"
                >
                  <X size={12} />
                </button>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

function ProgressBar({ pct }: { pct: number | null }) {
  return (
    <div className="h-1 w-24 shrink-0 overflow-hidden rounded bg-surface-2">
      <div
        className="h-full rounded"
        style={{
          width: pct != null ? `${pct}%` : "40%",
          background: "var(--color-accent)",
          transition: "width .2s",
        }}
      />
    </div>
  );
}

/** Live ETA from throughput: tracks `done` over wall-clock time (per `key`), smooths the rate with
 *  an EMA, and returns a formatted "time left" string — or null when it can't estimate (unknown
 *  total, no progress yet, or done). */
function useEta(key: string, done: number, total: number | null): string | null {
  const [eta, setEta] = useState<string | null>(null);
  const sample = useRef<{ t: number; done: number } | null>(null);
  const rate = useRef(0); // items per ms (EMA)

  useEffect(() => {
    // Reset the estimator when the tracked job/aggregate identity changes.
    sample.current = null;
    rate.current = 0;
    setEta(null);
  }, [key]);

  useEffect(() => {
    if (total == null || total <= 0) {
      setEta(null);
      return;
    }
    const now = Date.now();
    const prev = sample.current;
    if (prev && now > prev.t && done > prev.done) {
      const inst = (done - prev.done) / (now - prev.t);
      rate.current = rate.current === 0 ? inst : rate.current * 0.6 + inst * 0.4;
    }
    sample.current = { t: now, done };
    const remaining = Math.max(0, total - done);
    setEta(remaining > 0 && rate.current > 0 ? fmtDuration(remaining / rate.current) : null);
  }, [done, total]);

  return eta;
}

/** ms → a compact "1m 20s" / "45s" / "1h 3m" duration. */
function fmtDuration(ms: number): string {
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) {
    const rem = s % 60;
    return rem ? `${m}m ${rem}s` : `${m}m`;
  }
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}
