import { useCallback, useEffect, useRef, useState } from "react";
import { ChevronUp, EyeOff, History, Loader2, LogIn, LogOut, Server, ShieldCheck, X } from "lucide-react";
import { useNavigate } from "react-router";
import { useCancelJob, useJobs, useScopes, useStats, useVersion, useWhoami } from "@/api/queries";
import { authApi } from "@/api/auth";
import { useConnection, type ConnState } from "@/api/connection";
import type { JobStatus, MediaType } from "@/api/types";
import { useEta } from "@/lib/eta";
import { clearToken, getServer, isRemote, serverLabel } from "@/lib/server";
import { toast } from "@/lib/toast";
import { useEscape, useFocusTrap } from "@/lib/use-focus-trap";
import { ConnectDialog } from "./ConnectDialog";
import { AccountLoginForm } from "./auth/AccountLoginForm";
import { TokenLoginForm } from "./auth/TokenLoginForm";

/** Bottom status strip: active scan/analysis jobs with live progress, the live-connection state, and
 *  the server build. A single job shows inline; concurrent jobs condense into one aggregate bar with
 *  an overall percentage + ETA that expands to the individual jobs (issue #59). The connection dot
 *  makes a silent disconnect visible (issues #24, #25). */
export function StatusBar() {
  const version = useVersion();
  const jobs = useJobs({});
  const cancel = useCancelJob();
  const conn = useConnection();
  const navigate = useNavigate();
  const [cancelling, setCancelling] = useState<Set<string>>(() => new Set());

  const active = (jobs.data?.items ?? []).filter(
    (j) => j.state === "running" || j.state === "queued",
  );

  useEffect(() => {
    const activeIds = new Set(active.map((job) => job.id));
    setCancelling((current) => {
      const next = new Set([...current].filter((id) => activeIds.has(id)));
      return next.size === current.size ? current : next;
    });
  }, [active]);

  const cancelJob = (id: string) => {
    if (cancelling.has(id)) return;
    setCancelling((current) => new Set(current).add(id));
    cancel.mutate(id, {
      onError: () => {
        setCancelling((current) => {
          const next = new Set(current);
          next.delete(id);
          return next;
        });
      },
    });
  };

  return (
    <footer className="flex min-h-7 min-w-0 shrink-0 flex-wrap items-center gap-x-2 gap-y-1 border-t border-border bg-surface px-3 py-1 text-[11px] text-fg-dim lg:flex-nowrap lg:py-0 coarse:min-h-11 coarse:py-1">
      <div className="flex min-w-0 basis-full items-center lg:basis-auto lg:flex-1">
        {active.length === 0 ? (
          <span className="flex-1">Idle</span>
        ) : active.length === 1 ? (
          <JobPill
            job={active[0]}
            cancelling={cancelling.has(active[0].id)}
            onCancel={() => cancelJob(active[0].id)}
          />
        ) : (
          <AggregateJobs jobs={active} cancelling={cancelling} onCancel={cancelJob} />
        )}
      </div>
      <div className="flex min-w-0 flex-1 flex-wrap items-center justify-end gap-x-1 gap-y-0.5 lg:flex-initial lg:flex-nowrap lg:gap-x-2">
        <button
          type="button"
          className="flex shrink-0 items-center gap-1 rounded px-1.5 py-0.5 hover:text-fg coarse:min-h-11 coarse:min-w-11 coarse:justify-center"
          onClick={() => navigate("/jobs")}
          title="Open background job history"
          aria-label="Open background job history"
        >
          <History size={11} aria-hidden="true" />
          <span className="hidden sm:inline">History</span>
        </button>
        <MediaBreakdown />
        <ConnectionPill state={conn.state} />
        <IdentityChip auth={version.data?.auth} accounts={version.data?.accounts === true} />
        <ServerChip />
        <span className="hidden tabular-nums lg:inline">{version.data?.server ?? ""}</span>
      </div>
    </footer>
  );
}

/** App-level lifecycle observer. It stays mounted across routes so a job that finishes while the
 * user is in Settings or another report still produces a durable, linked notification. */
export function JobNotifications() {
  const jobs = useJobs({});
  const navigate = useNavigate();
  const openJob = useCallback(
    (id: string) => navigate(`/jobs?job=${encodeURIComponent(id)}`),
    [navigate],
  );
  return <JobAnnouncer jobs={jobs.data?.items ?? []} onOpenJob={openJob} />;
}

/** Sign-in / identity / sign-out chip (front-door auth). On a gated server (token or anonymous):
 *  - signed out → "Sign in" opens the sign-in form (anonymous browses read-only meanwhile);
 *  - signed in → the identity + scope hint, with a Sign out that drops the credential.
 *  With user accounts (issue #42) the credential may be a session cookie: the chip shows the
 *  account's username, Sign out also ends the server-side session, and a sharing-restricted view is
 *  flagged. Nothing shows when auth is off (there's no credential to manage). */
function IdentityChip({ auth, accounts }: { auth?: string; accounts: boolean }) {
  const [open, setOpen] = useState(false);
  const whoami = useWhoami();
  const scopes = useScopes();
  // Only manage a credential where the server actually has one (token/anonymous postures).
  if (auth !== "token" && auth !== "anonymous") return null;

  const account = whoami.data?.account ?? null;
  const restricted = whoami.data?.restricted === true;
  const nativeCredential = getServer().nativeCredential;
  const signedIn = !!getServer().token || nativeCredential || !!account;
  const canWrite = scopes.includes("write");
  const identity = account?.username ?? whoami.data?.identity ?? null;

  if (!signedIn) {
    return (
      <>
        <button
          className="flex items-center gap-1 rounded px-1.5 py-0.5 text-warn hover:text-fg coarse:min-h-11"
          onClick={() => setOpen(true)}
          title="Browsing read-only — sign in to make changes"
        >
          <LogIn size={11} />
          <span>
            <span className="hidden sm:inline">Signed out · </span>Sign in
          </span>
        </button>
        {open && <SignInModal accounts={accounts} onClose={() => setOpen(false)} />}
      </>
    );
  }

  const signOut = async () => {
    // End the server-side session when signed in via account (CSRF-stamped); best-effort — a dead
    // server shouldn't trap the user signed in. Then drop any stored token and reload so every
    // transport restarts unauthenticated and lands on the gate / read-only browsing.
    if (account) {
      try {
        await authApi.logout();
      } catch {
        /* best-effort — the reload lands on the gate either way */
      }
    }
    clearToken();
    location.reload();
  };

  return (
    <span className="flex min-w-0 items-center gap-1">
      <span
        className="flex min-w-0 items-center gap-1 text-fg-dim"
        title={
          canWrite
            ? "Signed in with write access"
            : "Signed in read-only — writes need a credential with write access"
        }
      >
        <ShieldCheck size={11} className={canWrite ? "text-accent" : "text-warn"} />
        <span className="max-w-[72px] truncate sm:max-w-[140px]">{identity ?? "Signed in"}</span>
        {!canWrite && <span className="hidden text-warn sm:inline">· read-only</span>}
      </span>
      {restricted && (
        <span
          className="flex items-center text-fg-dim"
          title="Some content may be hidden by sharing rules"
          aria-label="Some content may be hidden by sharing rules"
        >
          <EyeOff size={11} />
        </span>
      )}
      {nativeCredential ? (
        <span
          className="hidden text-fg-dim sm:inline"
          title={
            getServer().nativeCredentialForgettable
              ? "Use File → Forget Server Credential, then relaunch"
              : "This credential lasts only for the current desktop session"
          }
        >
          {getServer().nativeCredentialForgettable ? "File → Forget credential" : "Desktop session"}
        </span>
      ) : (
        <button
          className="flex items-center justify-center gap-1 rounded px-1.5 py-0.5 hover:text-fg coarse:min-h-11 coarse:min-w-11"
          onClick={() => void signOut()}
          aria-label="Sign out"
          title={
            account
              ? "Sign out — end this session (stays connected to this server)"
              : "Sign out — forget the tab token (stays connected to this server)"
          }
        >
          <LogOut size={11} />
          <span className="hidden sm:inline">Sign out</span>
        </button>
      )}
    </span>
  );
}

/** The dismissable sign-in dialog (front-door auth) — the sign-in form in a focus-trapped modal
 *  with Escape/backdrop to close. Shared shape with the boot gate; here it's optional and closable.
 *  Accounts-on servers get the username/password form (token reachable via its toggle). */
function SignInModal({ accounts, onClose }: { accounts: boolean; onClose: () => void }) {
  const ref = useFocusTrap<HTMLDivElement>(true);
  useEscape(onClose);
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
      onClick={onClose}
    >
      <div
        ref={ref}
        role="dialog"
        aria-modal="true"
        aria-labelledby="login-title"
        className="w-full max-w-md rounded-lg border border-border bg-surface p-4 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        {accounts ? (
          <AccountLoginForm reason={null} allowReadOnly={false} onClose={onClose} />
        ) : (
          <TokenLoginForm reason={null} allowReadOnly={false} onClose={onClose} />
        )}
      </div>
    </div>
  );
}

/** Connection target chip (hosted mode, issue #74): shows "Local" or the remote host, and opens the
 *  Connect dialog to point the client at a different 3DAM server. Mirrors the native GUI chip (#70). */
function ServerChip() {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button
        className={`flex min-w-0 items-center gap-1 rounded px-1.5 py-0.5 hover:text-fg coarse:min-h-11 ${
          isRemote() ? "text-accent" : "text-fg-dim"
        }`}
        onClick={() => setOpen(true)}
        title="Connect to a 3DAM server"
      >
        <Server size={11} />
        <span className="max-w-[72px] truncate sm:max-w-[160px]">{serverLabel()}</span>
      </button>
      {open && <ConnectDialog onClose={() => setOpen(false)} />}
    </>
  );
}

/** Per-media asset totals on the right of the bar — `3D 412 · IMG 590 · SFX 246` (issue #64,
 *  DESIGN_GUIDELINES footer). Same hue + short label per media type as the grid/table `MediaBadge`
 *  (model → 3D indigo, image → IMG orange, audio → SFX teal, video → VID rose, document → DOC
 *  slate); reads the already-cached `by_media` stats. Media types with no assets are omitted, so a
 *  library with no video or documents shows exactly what it showed before they existed. */
const MEDIA_BREAKDOWN: { key: MediaType; label: string; full: string; color: string }[] = [
  { key: "model", label: "3D", full: "3D models", color: "var(--color-media-model)" },
  { key: "image", label: "IMG", full: "images", color: "var(--color-media-image)" },
  { key: "audio", label: "SFX", full: "audio", color: "var(--color-media-audio)" },
  { key: "video", label: "VID", full: "video", color: "var(--color-media-video)" },
  { key: "document", label: "DOC", full: "documents", color: "var(--color-media-document)" },
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
 *  hidden live region speaks only lifecycle transitions rather than every progress tick. Cancelled
 *  jobs also raise a visible toast, so cancellation feedback isn't available only to AT users. */
function JobAnnouncer({
  jobs,
  onOpenJob,
}: {
  jobs: JobStatus[];
  onOpenJob: (id: string) => void;
}) {
  const [msg, setMsg] = useState("");
  const previous = useRef<Map<string, JobStatus["state"]> | null>(null);
  useEffect(() => {
    const next = new Map(jobs.map((job) => [job.id, job.state]));
    const transitions = jobs.flatMap((job) => {
      if (!previous.current) {
        return job.state === "queued" || job.state === "running"
          ? [`${job.kind} job started`]
          : [];
      }
      const oldState = previous.current?.get(job.id);
      if (oldState === job.state) return [];
      const action = { label: "View details", onClick: () => onOpenJob(job.id) };
      if (job.state === "done") {
        if ((job.warnings?.length ?? 0) > 0) {
          toast.warning(`${job.kind} job completed with warnings`, { action });
          return [`${job.kind} job completed with warnings`];
        }
        toast.success(`${job.kind} job completed`, { action });
        return [`${job.kind} job completed successfully`];
      }
      if (job.state === "failed") {
        toast.error(
          `${job.kind} job failed${job.error ? `: ${job.error}` : ""}`,
          { action },
        );
        return [`${job.kind} job failed`];
      }
      if (job.state === "cancelled") {
        toast.info(`${job.kind} job cancelled`, { action });
        return [`${job.kind} job cancelled`];
      }
      if (!oldState && (job.state === "queued" || job.state === "running")) {
        return [`${job.kind} job started`];
      }
      return [];
    });
    if (transitions.length) setMsg(transitions.join(". "));
    previous.current = next;
    // Keyed on states only: progress updates re-render the component but never change the message.
  }, [jobs, onOpenJob]);
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
  const accessibleLabel =
    state === "online"
      ? "Live updates connected"
      : state === "reconnecting"
        ? "Live updates paused, reconnecting"
        : "Server offline";
  return (
    <span
      className="flex shrink-0 items-center gap-1.5"
      role="status"
      aria-label={accessibleLabel}
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
function JobPill({
  job,
  cancelling,
  onCancel,
}: {
  job: JobStatus;
  cancelling: boolean;
  onCancel: () => void;
}) {
  const { done, total, current } = job.progress;
  const pct = total ? Math.min(100, Math.round((done / total) * 100)) : null;
  const eta = useEta(job.id, done, total ?? null);
  const label = `${job.kind} job${current ? `: ${current}` : ""}`;
  return (
    <div className="flex min-w-0 flex-1 items-center gap-2">
      <Loader2 size={12} className="shrink-0 animate-spin text-accent" aria-hidden="true" />
      <span className="max-w-16 truncate capitalize sm:max-w-none">{job.kind}</span>
      <ProgressBar pct={pct} label={label} done={done} total={total} current={current} />
      <span className="tabular-nums">{pct != null ? `${pct}%` : done.toLocaleString()}</span>
      {eta && <span className="hidden tabular-nums text-fg-dim sm:inline">~{eta} left</span>}
      {current && (
        <span className="hidden max-w-[220px] truncate text-fg-dim md:inline" title={current}>
          {current}
        </span>
      )}
      <button
        className="flex shrink-0 items-center justify-center gap-1 text-fg-dim hover:text-danger disabled:cursor-not-allowed disabled:opacity-60 coarse:min-h-11 coarse:min-w-11"
        onClick={onCancel}
        disabled={cancelling}
        aria-label={cancelling ? `Cancelling ${label}` : `Cancel ${label}`}
      >
        {cancelling ? (
          <span className="text-[10px]">Cancelling…</span>
        ) : (
          <X size={12} aria-hidden="true" />
        )}
      </button>
    </div>
  );
}

/** Concurrent jobs condensed into one bar: aggregate done/total across all active jobs, an overall %
 *  and ETA, expandable to the individual jobs (each cancellable). */
function AggregateJobs({
  jobs,
  cancelling,
  onCancel,
}: {
  jobs: JobStatus[];
  cancelling: Set<string>;
  onCancel: (id: string) => void;
}) {
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
      <Loader2 size={12} className="shrink-0 animate-spin text-accent" aria-hidden="true" />
      <button
        className="flex items-center gap-1 capitalize hover:text-fg coarse:min-h-11"
        onClick={() => setOpen((o) => !o)}
        title="Show individual jobs"
        aria-expanded={open}
      >
        <span className="max-w-16 truncate sm:max-w-none">{label}</span>
        <ChevronUp
          size={11}
          className="transition-transform"
          style={{ transform: open ? "none" : "rotate(180deg)" }}
        />
      </button>
      <ProgressBar
        pct={pct}
        label={`${label} aggregate progress`}
        done={done}
        total={total}
      />
      <span className="tabular-nums">
        {pct != null ? `${pct}%` : `${done.toLocaleString()} done`}
      </span>
      {eta && <span className="hidden tabular-nums text-fg-dim sm:inline">~{eta} left</span>}

      {open && (
        <div className="absolute bottom-full left-0 mb-1.5 flex max-h-64 w-[calc(100vw-1rem)] max-w-[380px] flex-col gap-1 overflow-y-auto rounded-md border border-border bg-surface p-1.5 shadow-xl">
          {jobs.map((j) => {
            const p = j.progress.total
              ? Math.min(100, Math.round((j.progress.done / j.progress.total) * 100))
              : null;
            const jobLabel = `${j.kind} job${j.progress.current ? `: ${j.progress.current}` : ""}`;
            const isCancelling = cancelling.has(j.id);
            return (
              <div
                key={j.id}
                className="flex items-center gap-2 rounded px-1.5 py-1 hover:bg-surface-2"
              >
                <span className="w-16 shrink-0 capitalize text-fg-muted">{j.kind}</span>
                <ProgressBar
                  pct={p}
                  label={jobLabel}
                  done={j.progress.done}
                  total={j.progress.total}
                  current={j.progress.current}
                />
                <span className="w-9 shrink-0 text-right tabular-nums">
                  {p != null ? `${p}%` : j.progress.done.toLocaleString()}
                </span>
                {j.progress.current && (
                  <span className="min-w-0 flex-1 truncate text-fg-dim" title={j.progress.current}>
                    {j.progress.current}
                  </span>
                )}
                <button
                  className="flex shrink-0 items-center justify-center gap-1 text-fg-dim hover:text-danger disabled:cursor-not-allowed disabled:opacity-60 coarse:min-h-11 coarse:min-w-11"
                  onClick={() => onCancel(j.id)}
                  disabled={isCancelling}
                  aria-label={isCancelling ? `Cancelling ${jobLabel}` : `Cancel ${jobLabel}`}
                >
                  {isCancelling ? (
                    <span className="text-[10px]">Cancelling…</span>
                  ) : (
                    <X size={12} aria-hidden="true" />
                  )}
                </button>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

function ProgressBar({
  pct,
  label,
  done,
  total,
  current,
}: {
  pct: number | null;
  label: string;
  done: number;
  total: number | null;
  current?: string | null;
}) {
  const determinate = total != null && total > 0;
  const valueText =
    determinate
      ? `${done.toLocaleString()} of ${total.toLocaleString()}${current ? `, ${current}` : ""}`
      : `${done.toLocaleString()} complete${current ? `, ${current}` : ""}`;
  return (
    <div
      className="h-1 w-14 shrink-0 overflow-hidden rounded bg-surface-2 sm:w-24"
      role="progressbar"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={determinate ? total : 100}
      aria-valuenow={determinate ? Math.min(done, total) : undefined}
      aria-valuetext={determinate ? valueText : `Indeterminate, ${valueText}`}
    >
      <div
        aria-hidden="true"
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
