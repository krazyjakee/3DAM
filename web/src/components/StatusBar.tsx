import { Loader2, X } from "lucide-react";
import { useCancelJob, useJobs, useVersion } from "@/api/queries";
import type { JobStatus } from "@/api/types";

/** Bottom status strip: active scan/analysis jobs with live progress + the server build. */
export function StatusBar() {
  const version = useVersion();
  const jobs = useJobs({});
  const cancel = useCancelJob();

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
      <span className="tabular-nums">{version.data?.server ?? ""}</span>
    </footer>
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
