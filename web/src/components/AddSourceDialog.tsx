import { useEffect, useState, type ReactNode } from "react";
import { FolderPlus, X } from "lucide-react";
import { useAddSource, useScan } from "@/api/queries";
import { ApiError } from "@/api/client";
import { useFocusTrap } from "@/lib/use-focus-trap";
import type { SourceKind, SourceOptions } from "@/api/types";

// local_fs, sftp and smb are all backed by the phase-4 engine; federated peers are not built yet, so
// they stay disabled rather than offering a dead option.
const KINDS: { value: SourceKind; label: string; enabled: boolean }[] = [
  { value: "local_fs", label: "Local folder", enabled: true },
  { value: "sftp", label: "SFTP", enabled: true },
  { value: "smb", label: "SMB / Samba", enabled: true },
  { value: "federated", label: "Federated peer (soon)", enabled: false },
];

// Per-kind copy for the primary URI/path input.
const URI_FIELD: Record<SourceKind, { label: string; placeholder: string }> = {
  local_fs: { label: "Path", placeholder: "/mnt/assets/sfx" },
  sftp: { label: "SFTP URI", placeholder: "sftp://user@host:22/path/to/assets" },
  smb: { label: "SMB URI", placeholder: "smb://host/share/path" },
  federated: { label: "Peer", placeholder: "" },
};

export function AddSourceDialog({ onClose }: { onClose: () => void }) {
  const add = useAddSource();
  const scan = useScan();
  const [kind, setKind] = useState<SourceKind>("local_fs");
  const [uri, setUri] = useState("");
  const [name, setName] = useState("");
  const [watch, setWatch] = useState(true);
  const [err, setErr] = useState<string | null>(null);

  // Connection auth for network sources. Values here override any userinfo in the URI. Kept in one
  // bag so unused fields for the current kind are simply not sent.
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [privateKey, setPrivateKey] = useState("");
  const [passphrase, setPassphrase] = useState("");
  const [domain, setDomain] = useState("");
  const [port, setPort] = useState("");

  const remote = kind === "sftp" || kind === "smb";

  // Modal focus management + Escape-to-close (issue #26): trap Tab within the dialog, return focus
  // to the trigger on close, and close on Escape (mirroring the Drawer, which already did).
  const dialogRef = useFocusTrap<HTMLDivElement>(true);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const submit = async () => {
    setErr(null);
    if (!uri.trim()) return setErr(`A ${URI_FIELD[kind].label.toLowerCase()} is required.`);

    const options: SourceOptions = { watch };
    if (remote) {
      // Only attach the credentials that were actually filled in — the backend prefers these over
      // anything parsed from the URI and ignores empties.
      const trim = (s: string) => s.trim();
      if (trim(username)) options.username = trim(username);
      if (password) options.password = password;
      if (kind === "sftp") {
        if (trim(privateKey)) options.private_key = trim(privateKey);
        if (passphrase) options.passphrase = passphrase;
      }
      if (kind === "smb" && trim(domain)) options.domain = trim(domain);
      if (trim(port)) {
        const p = Number(port);
        if (!Number.isInteger(p) || p < 1 || p > 65535)
          return setErr("Port must be between 1 and 65535.");
        options.port = p;
      }
    }

    try {
      const { id } = await add.mutateAsync({
        kind,
        uri: uri.trim(),
        name: name.trim() || null,
        options,
      });
      // add_source does not auto-scan — kick a full scan of the new source (tech-spec 03). Fire it
      // and forget: the scan is a background job, so the modal must NOT wait for it to finish before
      // closing. Awaiting it here serialised the UI — a second source couldn't be queued until the
      // first had fully scanned (issue #1). Any scan-submit failure surfaces as a toast (#23).
      scan.mutate({ sources: [id], mode: "full" });
      onClose();
    } catch (e) {
      setErr(e instanceof ApiError ? e.message : String(e));
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4"
      onClick={onClose}
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby="add-source-title"
        className="max-h-[90vh] w-full max-w-[380px] overflow-y-auto rounded-lg border border-border bg-surface p-4 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-3 flex items-center justify-between">
          <h2 id="add-source-title" className="flex items-center gap-2 text-sm font-semibold">
            <FolderPlus size={15} /> Add source
          </h2>
          <button
            className="flex items-center justify-center text-fg-dim hover:text-fg coarse:min-h-11 coarse:min-w-11"
            onClick={onClose}
            aria-label="Close"
          >
            <X size={16} />
          </button>
        </div>

        <label className="mb-1 block text-[11px] text-fg-muted">Kind</label>
        <select
          className="field mb-3 coarse:min-h-11"
          value={kind}
          onChange={(e) => setKind(e.target.value as SourceKind)}
        >
          {KINDS.map((k) => (
            <option key={k.value} value={k.value} disabled={!k.enabled}>
              {k.label}
            </option>
          ))}
        </select>

        <label className="mb-1 block text-[11px] text-fg-muted">{URI_FIELD[kind].label}</label>
        <input
          className="field mb-3 coarse:min-h-11"
          placeholder={URI_FIELD[kind].placeholder}
          value={uri}
          onChange={(e) => setUri(e.target.value)}
          autoFocus
          onKeyDown={(e) => e.key === "Enter" && submit()}
        />

        {remote && (
          <>
            <Field label="Username (optional)">
              <input
                className="field coarse:min-h-11"
                placeholder="overrides user@ in the URI"
                value={username}
                onChange={(e) => setUsername(e.target.value)}
                autoComplete="off"
              />
            </Field>
            <Field label="Password (optional)">
              <input
                className="field coarse:min-h-11"
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                autoComplete="new-password"
              />
            </Field>
            {kind === "sftp" && (
              <>
                <Field label="Private key path (optional)">
                  <input
                    className="field coarse:min-h-11"
                    placeholder="~/.ssh/id_ed25519"
                    value={privateKey}
                    onChange={(e) => setPrivateKey(e.target.value)}
                  />
                </Field>
                <Field label="Key passphrase (optional)">
                  <input
                    className="field coarse:min-h-11"
                    type="password"
                    value={passphrase}
                    onChange={(e) => setPassphrase(e.target.value)}
                    autoComplete="new-password"
                  />
                </Field>
              </>
            )}
            {kind === "smb" && (
              <Field label="Domain / workgroup (optional)">
                <input
                  className="field coarse:min-h-11"
                  placeholder="WORKGROUP"
                  value={domain}
                  onChange={(e) => setDomain(e.target.value)}
                />
              </Field>
            )}
            <Field label={`Port (optional, default ${kind === "sftp" ? 22 : 445})`}>
              <input
                className="field coarse:min-h-11"
                type="number"
                inputMode="numeric"
                min={1}
                max={65535}
                value={port}
                onChange={(e) => setPort(e.target.value)}
              />
            </Field>
          </>
        )}

        <label className="mb-1 block text-[11px] text-fg-muted">Name (optional)</label>
        <input
          className="field mb-3 coarse:min-h-11"
          placeholder="SFX library"
          value={name}
          onChange={(e) => setName(e.target.value)}
        />

        <label className="mb-3 flex items-center gap-2 text-xs text-fg-muted select-none coarse:min-h-11">
          <input
            type="checkbox"
            className="coarse:h-5 coarse:w-5"
            checked={watch}
            onChange={(e) => setWatch(e.target.checked)}
          />
          Watch for changes and re-scan deltas
        </label>

        {err && <p className="mb-3 text-xs text-danger">{err}</p>}

        <div className="flex justify-end gap-2">
          <button className="btn coarse:min-h-11" onClick={onClose}>
            Cancel
          </button>
          <button
            className="btn btn-accent coarse:min-h-11"
            onClick={submit}
            disabled={add.isPending}
          >
            {add.isPending ? "Adding…" : "Add & scan"}
          </button>
        </div>
      </div>
    </div>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="mb-3">
      <label className="mb-1 block text-[11px] text-fg-muted">{label}</label>
      {children}
    </div>
  );
}
