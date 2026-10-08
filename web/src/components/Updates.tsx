import { useEffect, useRef, useState } from "react";
import { Link, useLocation } from "react-router";
import { ArrowDownToLine, CheckCircle2, RefreshCw } from "lucide-react";
import {
  hasDesktopUpdater, RELEASES_URL, restartAfterUpdate, updateBusy,
  useDesktopUpdates, useUpdateAction,
} from "../api/updates";
import type { UpdatePhase } from "../api/types";
import { useDialogs } from "../lib/dialogs";
import { errorMessage } from "../lib/toast";

const PHASE_LABEL: Record<UpdatePhase, string> = {
  idle: "Ready to check for updates",
  checking: "Checking for updates…",
  up_to_date: "You’re up to date",
  available: "An update is available",
  downloading: "Downloading and verifying update…",
  installing: "Installing update…",
  ready: "Update installed — restart to finish",
  error: "Update could not be completed",
};

export function Updates() {
  const query = useDesktopUpdates();
  const check = useUpdateAction("desktop_update_check");
  const install = useUpdateAction("desktop_update_install");
  const preferences = useUpdateAction("desktop_update_preferences");
  const { confirm } = useDialogs();
  const location = useLocation();
  const requestedCheck = useRef(false);
  const [restartError, setRestartError] = useState<string | null>(null);
  const [restarting, setRestarting] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const status = query.data;
  const busy = updateBusy(status) || check.isPending || install.isPending;
  const manualCheck = new URLSearchParams(location.search).get("check") === "1";
  const runCheck = check.mutate;

  useEffect(() => {
    if (manualCheck && status?.supported && !busy && status.phase !== "ready" && !requestedCheck.current) {
      requestedCheck.current = true;
      runCheck(undefined);
    }
  }, [manualCheck, status, busy, runCheck]);

  async function installUpdate() {
    setConfirming(true);
    const accepted = await confirm({
      title: `Install 3DAM ${status?.release?.version}?`,
      message: "The update will be downloaded and its signature verified before installation. Finish active jobs and save your work first. Windows may close the app during installation; on Linux and macOS you can restart when ready.",
      confirmLabel: "Download and install",
    });
    setConfirming(false);
    if (accepted) install.mutate(undefined);
  }

  async function restart() {
    setRestartError(null);
    setRestarting(true);
    try {
      await restartAfterUpdate();
    } catch (error) {
      setRestartError(errorMessage(error));
      setRestarting(false);
    }
  }

  const error = status?.error ?? (query.error && errorMessage(query.error))
    ?? (check.error && errorMessage(check.error)) ?? (install.error && errorMessage(install.error))
    ?? (preferences.error && errorMessage(preferences.error)) ?? restartError;
  const percent = status?.total_bytes
    ? Math.min(100, Math.round(status.downloaded_bytes / status.total_bytes * 100)) : undefined;

  return (
    <main className="mx-auto flex w-full max-w-3xl flex-col gap-6 overflow-y-auto p-4 text-sm sm:p-6">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <h1 className="flex items-center gap-2 text-lg font-semibold"><ArrowDownToLine size={20} /> Updates</h1>
        <Link to="/" className="text-xs text-accent hover:underline">← Back to library</Link>
      </header>
      {!hasDesktopUpdater() ? (
        <section className="rounded-lg border border-border bg-surface p-5">
          <h2 className="font-medium">Desktop app updates</h2>
          <p className="mt-2 text-fg-muted">Open 3DAM with its local library to check for and install desktop updates. In a browser, the web client updates with its server.</p>
        </section>
      ) : !status ? (
        <section className="rounded-lg border border-border p-5" role="status">
          {query.error ? "Could not load updater status." : "Loading updater status…"}
          {query.error && <button className="btn ml-3" onClick={() => void query.refetch()}>Retry</button>}
        </section>
      ) : (
        <>
          <section className="rounded-lg border border-border bg-surface p-5" aria-labelledby="update-status-title">
            <p className="mb-3 text-xs text-fg-dim">Installed desktop version <span className="font-mono text-fg">{status.current_version}</span> · Stable channel</p>
            <h2 id="update-status-title" className="flex items-center gap-2 text-base font-medium" role="status" aria-live="polite">
              {status.phase === "up_to_date" && <CheckCircle2 size={18} className="text-accent" />}
              {status.supported ? PHASE_LABEL[status.phase] : "Manual update required"}
            </h2>
            {status.unavailable_reason && <p className="mt-2 text-fg-muted">{status.unavailable_reason}</p>}
            {status.checked_at && <p className="mt-2 text-xs text-fg-dim">Last successful check: {new Date(status.checked_at).toLocaleString()}</p>}
            {status.phase === "downloading" && (
              <div className="mt-4">
                <progress className="w-full accent-accent" aria-label="Update download" max={100} value={percent} />
                <p className="mt-1 text-xs text-fg-muted">{percent !== undefined ? `${percent}% · ` : ""}{(status.downloaded_bytes / 1024 / 1024).toFixed(1)} MB downloaded</p>
              </div>
            )}
            {status.supported && (
              <div className="mt-4 flex flex-wrap gap-2">
                {status.phase !== "ready" && <button className="btn" disabled={busy || confirming} onClick={() => check.mutate(undefined)}><RefreshCw size={14} className={status.phase === "checking" ? "animate-spin" : ""} /> Check for updates</button>}
                {status.release && ["available", "error"].includes(status.phase) && <button className="btn btn-accent" disabled={busy || confirming} onClick={() => void installUpdate()}>Download and install {status.release.version}</button>}
                {status.phase === "ready" && <button className="btn btn-accent" disabled={restarting} onClick={() => void restart()}>{restarting ? "Restarting…" : "Restart 3DAM"}</button>}
              </div>
            )}
          </section>
          {status.release && (
            <section className="rounded-lg border border-border p-5" aria-labelledby="release-notes-title">
              <h2 id="release-notes-title" className="font-medium">What’s new in {status.release.version}</h2>
              <p className="mt-3 whitespace-pre-wrap break-words text-fg-muted">{status.release.notes || "No release notes were provided for this update."}</p>
            </section>
          )}
          {status.supported && (
            <section className="rounded-lg border border-border p-5">
              <label className="flex items-start gap-3">
                <input type="checkbox" className="mt-1 accent-accent" checked={status.automatic_checks} disabled={preferences.isPending} onChange={(event) => preferences.mutate({ automaticChecks: event.target.checked })} />
                <span><span className="font-medium">Automatically check for updates</span><span className="mt-1 block text-xs text-fg-dim">Check at launch and every six hours. Downloads and installation start only when you choose.</span></span>
              </label>
            </section>
          )}
        </>
      )}
      {error && <p role="alert" className="rounded border border-danger/40 bg-danger/10 p-3 text-danger">{error}</p>}
      <p className="text-xs text-fg-dim">Updates are verified with the publisher’s signing key. <a href={RELEASES_URL} target="_blank" rel="noreferrer" className="text-accent hover:underline">View releases and downloads ↗</a></p>
    </main>
  );
}
