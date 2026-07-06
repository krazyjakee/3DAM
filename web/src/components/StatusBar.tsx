import { Loader2, X } from "lucide-react";
import { useCancelJob, useJobs, useVersion } from "@/api/queries";
import { useConnection, type ConnState } from "@/api/connection";
import type { JobStatus } from "@/api/types";

/** Bottom status strip: active scan/analysis jobs with live progress, the live-connection state, and
 *  the server build. The connection dot makes a silent disconnect visible (issues #24, #25). */
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
      {active.length > 0 ? (
        <div className="flex min-w-0 flex-1 items-center gap-4">
          {active.slice(0, 3).map((j) => (
            <JobPill key={j.id} job={j} onCancel={() => cancel.mutate(j.id)} />
          ))}
          {active.length > 3 && <span>+{active.length - 3} more</span>}
        </div>
      ) : (
        <span className="flex-1">Idle</span>
      )}
      <ConnectionPill state={conn.state} />
      <span className="tabular-nums">{version.data?.server ?? ""}</span>
    </footer>
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

function JobPill({ job, onCancel }: { job: JobStatus; onCancel: () => void }) {
  const { done, total, current } = job.progress;
  const pct = total ? Math.min(100, Math.round((done / total) * 100)) : null;
  return (
    <div className="flex min-w-0 items-center gap-2">
      <Loader2 size={12} className="animate-spin text-accent" />
      <span className="capitalize">{job.kind}</span>
      <div className="h-1 w-24 overflow-hidden rounded bg-surface-2">
        <div
          className="h-full rounded"
          style={{
            width: pct != null ? `${pct}%` : "40%",
            background: "var(--color-accent)",
            transition: "width .2s",
          }}
        />
      </div>
      <span className="tabular-nums">
        {pct != null ? `${pct}%` : done.toLocaleString()}
      </span>
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
