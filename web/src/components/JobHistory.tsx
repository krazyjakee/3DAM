import { useEffect, useMemo } from "react";
import { AlertTriangle, Ban, CheckCircle2, CircleX, Clock3, History } from "lucide-react";
import { Link, useSearchParams } from "react-router";
import { useJobHistory, useSources } from "@/api/queries";
import type { JobStatus } from "@/api/types";
import { CenteredCard } from "@/lib/ui";

/** Persisted job list + terminal report. The server performs visibility filtering before returning
 * every page; this client deliberately shows source names, never reconstructs connection paths. */
export function JobHistory() {
  const history = useJobHistory();
  const sources = useSources();
  const [params, setParams] = useSearchParams();
  const jobs = useMemo(() => history.data?.pages.flatMap((page) => page.items) ?? [], [history.data]);
  const selectedId = params.get("job");
  const selected = jobs.find((job) => job.id === selectedId) ?? null;
  const { fetchNextPage, hasNextPage, isFetchingNextPage } = history;
  const sourceNames = useMemo(
    () => new Map((sources.data ?? []).map((source) => [source.id, source.name])),
    [sources.data],
  );

  // A notification can deep-link to an older page. Fetch sequentially until it is found or the
  // caller's visible history is exhausted; the detail therefore survives both reload and navigation.
  useEffect(() => {
    if (!selectedId || selected || !hasNextPage || isFetchingNextPage) return;
    void fetchNextPage();
  }, [fetchNextPage, hasNextPage, isFetchingNextPage, selected, selectedId]);

  return (
    <div className="mx-auto flex min-h-dvh max-w-5xl flex-col gap-5 p-6 text-sm">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-2">
          <History size={18} className="text-accent" aria-hidden="true" />
          <h1 className="text-lg font-semibold text-fg">Background job history</h1>
        </div>
        <Link to="/" className="text-accent hover:underline">
          ← Back to library
        </Link>
      </header>

      <p className="text-xs text-fg-dim">
        Progress and terminal outcomes remain here after reload. Partial results retain their
        warnings; failed and cancelled work is never presented as successful.
      </p>

      {history.isLoading ? (
        <CenteredCard>Loading job history…</CenteredCard>
      ) : history.isError ? (
        <CenteredCard tone="danger">Couldn’t load job history.</CenteredCard>
      ) : jobs.length === 0 ? (
        <CenteredCard>No background jobs have run yet.</CenteredCard>
      ) : (
        <div className="grid min-h-0 gap-4 md:grid-cols-[minmax(280px,0.9fr)_minmax(340px,1.1fr)]">
          <section
            aria-label="Jobs"
            className="overflow-hidden rounded border border-border bg-surface"
          >
            <ul className="divide-y divide-border">
              {jobs.map((job) => (
                <li key={job.id}>
                  <button
                    type="button"
                    onClick={() => setParams({ job: job.id })}
                    aria-current={selectedId === job.id ? "true" : undefined}
                    className={`flex w-full items-start gap-3 px-3 py-3 text-left hover:bg-surface-2 ${
                      selectedId === job.id ? "bg-accent-muted" : ""
                    }`}
                  >
                    <OutcomeIcon job={job} />
                    <span className="min-w-0 flex-1">
                      <span className="flex items-center justify-between gap-2">
                        <span className="capitalize text-fg">{job.kind}</span>
                        <span className="shrink-0 text-[10px] capitalize text-fg-dim">
                          {outcomeLabel(job)}
                        </span>
                      </span>
                      <span className="mt-0.5 block truncate text-xs text-fg-muted">
                        {job.error ?? job.summary ?? progressSummary(job)}
                      </span>
                      <span className="mt-1 block text-[10px] text-fg-dim tabular-nums">
                        {formatTimestamp(job.updated_at ?? job.created_at)}
                      </span>
                    </span>
                  </button>
                </li>
              ))}
            </ul>
            {history.hasNextPage && (
              <button
                type="button"
                className="w-full border-t border-border px-3 py-2 text-xs text-accent hover:bg-surface-2 disabled:opacity-50"
                onClick={() => void history.fetchNextPage()}
                disabled={history.isFetchingNextPage}
              >
                {history.isFetchingNextPage ? "Loading…" : "Load older jobs"}
              </button>
            )}
          </section>

          <section aria-label="Job details">
            {selected ? (
              <JobDetails job={selected} sources={sourceNames} />
            ) : selectedId && history.hasNextPage ? (
              <CenteredCard>Finding job…</CenteredCard>
            ) : selectedId ? (
              <CenteredCard tone="danger">
                This job is unavailable or outside your library visibility.
              </CenteredCard>
            ) : (
              <CenteredCard>Select a job to inspect its report.</CenteredCard>
            )}
          </section>
        </div>
      )}
    </div>
  );
}

function JobDetails({ job, sources }: { job: JobStatus; sources: Map<string, string> }) {
  const warnings = job.warnings ?? [];
  const artifacts = job.result_artifacts ?? [];
  return (
    <article
      className="rounded border border-border bg-surface p-4"
      aria-labelledby="job-detail-title"
    >
      <div className="flex items-start justify-between gap-3">
        <div>
          <h2 id="job-detail-title" className="text-base font-semibold capitalize text-fg">
            {job.kind} job
          </h2>
          <p className="mt-0.5 font-mono text-[10px] text-fg-dim">{job.id}</p>
        </div>
        <span
          className="flex items-center gap-1.5 text-xs capitalize"
          style={{ color: outcomeColor(job) }}
        >
          <OutcomeIcon job={job} /> {outcomeLabel(job)}
        </span>
      </div>

      <dl className="mt-4 grid grid-cols-[max-content_1fr] gap-x-4 gap-y-2 text-xs">
        <dt className="text-fg-dim">Started</dt>
        <dd className="text-fg">{formatTimestamp(job.created_at)}</dd>
        <dt className="text-fg-dim">Last update</dt>
        <dd className="text-fg">{formatTimestamp(job.updated_at)}</dd>
        {job.initiator && (
          <>
            <dt className="text-fg-dim">Initiated by</dt>
            <dd className="text-fg">{job.initiator}</dd>
          </>
        )}
        <dt className="text-fg-dim">Progress</dt>
        <dd className="text-fg">{progressSummary(job)}</dd>
        <dt className="text-fg-dim">Sources</dt>
        <dd className="text-fg">
          {job.sources.length > 0
            ? job.sources.map((id) => sources.get(id) ?? "Visible source").join(", ")
            : "Not attributed"}
        </dd>
      </dl>

      {job.summary && (
        <section className="mt-4 border-t border-border pt-3">
          <h3 className="text-xs font-medium text-fg-muted">Summary</h3>
          <p className="mt-1 text-xs text-fg">{job.summary}</p>
        </section>
      )}
      {warnings.length > 0 && (
        <section className="mt-4 rounded border border-warn/40 bg-surface-2 p-3">
          <h3 className="flex items-center gap-1.5 text-xs font-medium text-warn">
            <AlertTriangle size={13} aria-hidden="true" /> Warnings
          </h3>
          <ul className="mt-2 list-disc space-y-1 pl-4 text-xs text-fg-muted">
            {warnings.map((warning, index) => (
              <li key={`${index}-${warning}`}>{warning}</li>
            ))}
          </ul>
        </section>
      )}
      {job.error && (
        <section className="mt-4 rounded border border-danger/40 bg-surface-2 p-3">
          <h3 className="text-xs font-medium text-danger">Error</h3>
          <p className="mt-1 whitespace-pre-wrap break-words text-xs text-fg">{job.error}</p>
        </section>
      )}
      {artifacts.length > 0 && (
        <section className="mt-4 border-t border-border pt-3">
          <h3 className="text-xs font-medium text-fg-muted">Result artifacts</h3>
          <ul className="mt-2 space-y-1 text-xs">
            {artifacts.map((artifact, index) => (
              <li key={`${index}-${artifact.label}`}>
                {artifact.route ? (
                  <Link to={artifact.route} className="text-accent hover:underline">
                    {artifact.label}
                  </Link>
                ) : (
                  <span className="text-fg">{artifact.label}</span>
                )}
              </li>
            ))}
          </ul>
        </section>
      )}
    </article>
  );
}

function isPartial(job: JobStatus): boolean {
  return job.state === "done" && (job.warnings?.length ?? 0) > 0;
}

function outcomeLabel(job: JobStatus): string {
  if (isPartial(job)) return "partial";
  return job.state === "done" ? "completed" : job.state;
}

function outcomeColor(job: JobStatus): string {
  if (isPartial(job)) return "var(--color-warn)";
  if (job.state === "failed") return "var(--color-danger)";
  if (job.state === "done") return "var(--color-lic-permissive)";
  return "var(--color-fg-muted)";
}

function OutcomeIcon({ job }: { job: JobStatus }) {
  const common = "mt-0.5 shrink-0";
  if (isPartial(job)) {
    return <AlertTriangle size={15} className={`${common} text-warn`} aria-hidden="true" />;
  }
  if (job.state === "failed") {
    return <CircleX size={15} className={`${common} text-danger`} aria-hidden="true" />;
  }
  if (job.state === "cancelled") {
    return <Ban size={15} className={`${common} text-fg-muted`} aria-hidden="true" />;
  }
  if (job.state === "done") {
    return (
      <CheckCircle2
        size={15}
        className={common}
        style={{ color: "var(--color-lic-permissive)" }}
        aria-hidden="true"
      />
    );
  }
  return <Clock3 size={15} className={`${common} text-accent`} aria-hidden="true" />;
}

function progressSummary(job: JobStatus): string {
  const done = job.progress.done.toLocaleString();
  return job.progress.total == null
    ? `${done} item(s) processed`
    : `${done} of ${job.progress.total.toLocaleString()} item(s)`;
}

function formatTimestamp(value?: number): string {
  return value ? new Date(value).toLocaleString() : "Unavailable";
}
