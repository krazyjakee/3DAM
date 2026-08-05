// The Inspector's licence block (issue #166, parent #96) — extracted from `Inspector.tsx`. One
// mutation (`useSetLicense`) and one `asset` prop; `web/tests/components/license-editing.test.tsx`
// is its regression guard.
//
// The editor stays one component: it is a single `LicenseDraft` reducer-shaped form, and every
// field is a field *of that draft*. Splitting it per input would scatter one state shape across
// modules for no reader's benefit.

import { useState } from "react";
import { Pencil } from "lucide-react";
import { useCan, useSetLicense } from "@/api/queries";
import { ApiError } from "@/api/client";
import type { Asset, LicenseEditResult, LicenseInput } from "@/api/types";
import { AUTH_COPY } from "@/lib/auth";
import {
  LICENSE_PRESETS,
  RIGHTS_FIELDS,
  textPatch,
  TRI_OPTIONS,
  triFromBool,
  triToBool,
  type TriState,
} from "@/lib/license";
import { peerReadOnlyTitle } from "@/lib/origin";
import { LicenseBadge } from "../LicenseBadge";
import { Field } from "./primitives";

/** The licence block: the derived badge, the rights readout, and the editor behind it (issue #106).
 *
 *  Peer-owned assets are read-only references (tech-spec 07 §7.4) — their rights are managed on the
 *  owning peer and attributed to it here, never edited through this server. */
export function LicenseSection({ asset }: { asset: Asset }) {
  const { license } = asset;
  const canWrite = useCan("write");
  const peerTitle = peerReadOnlyTitle(asset.summary.origin);
  const readOnly = !canWrite || !!peerTitle;
  const [editing, setEditing] = useState(false);
  /** The status the *server* derived on the last save — never a locally guessed one. */
  const [derived, setDerived] = useState<LicenseEditResult | null>(null);

  return (
    <div className="mt-2">
      <div className="flex items-start justify-between gap-2">
        <LicenseBadge badge={{ id: license.id, status: license.status }} prominent />
        {!editing && (
          <button
            type="button"
            className="btn shrink-0 px-1.5 py-1 disabled:cursor-not-allowed disabled:opacity-40 coarse:min-h-11"
            disabled={readOnly}
            title={
              peerTitle ??
              (!canWrite ? AUTH_COPY.needsWrite : "Edit licence and usage rights")
            }
            aria-label="Edit licence"
            onClick={() => {
              setDerived(null);
              setEditing(true);
            }}
          >
            <Pencil size={12} /> Licence
          </button>
        )}
      </div>
      {typeof asset.summary.origin !== "string" && (
        <p className="mt-1 text-[10px] text-fg-dim">
          Licence recorded by peer “{asset.summary.origin.peer}” — read-only here. Federated rights
          are attributed to their owner and managed there.
        </p>
      )}
      {editing ? (
        <LicenseEditor
          asset={asset}
          onDone={(result) => {
            setDerived(result);
            setEditing(false);
          }}
          onCancel={() => setEditing(false)}
        />
      ) : (
        <>
          <Rights license={license} />
          {derived && <DerivedStatus result={derived} />}
        </>
      )}
    </div>
  );
}

/** What the server derived from the id + rights it just stored (tech-spec 02 §5). Shown verbatim so
 *  the user learns the rule — notably that "permissive" is unreachable without a named licence and
 *  four known rights. */
function DerivedStatus({ result }: { result: LicenseEditResult }) {
  return (
    <div className="mt-2 rounded border border-border bg-surface-2 p-2 text-[11px] text-fg-muted">
      <div className="flex flex-wrap items-center gap-1.5">
        <span>Saved — status now</span>
        {result.status.length === 0 ? (
          <span className="text-fg-dim">unchanged</span>
        ) : (
          result.status.map((entry) => (
            <span key={entry.status} className="inline-flex items-center gap-1">
              <LicenseBadge badge={{ id: null, status: entry.status }} />
              {result.status.length > 1 && <span className="tabular-nums">×{entry.count}</span>}
            </span>
          ))
        )}
      </div>
      {result.warnings.map((warning, index) => (
        <p key={`${warning.code}:${warning.subject}:${index}`} className="mt-1 text-warn">
          {warning.message}
        </p>
      ))}
    </div>
  );
}

function Rights({ license }: { license: Asset["license"] }) {
  const flags: [string, boolean | null][] = [
    ["Commercial", license.commercial],
    ["Modify", license.modify],
    ["Redistribute", license.redistribute],
    ["Attribution", license.attribution],
  ];
  const known = flags.filter(([, v]) => v !== null);
  if (known.length === 0 && !license.holder && !license.credit && !license.url) return null;
  return (
    <div className="mt-2 space-y-1">
      {license.holder && <Field label="Holder" value={license.holder} />}
      {license.credit && <Field label="Credit" value={license.credit} />}
      {license.url && <Field label="Source" value={license.url} copyable />}
      {known.length > 0 && (
        <div className="flex flex-wrap gap-1 pt-1">
          {known.map(([label, v]) => (
            <span
              key={label}
              className="rounded px-1.5 py-0.5 text-[10px]"
              style={{
                color: v ? "var(--color-lic-permissive)" : "var(--color-lic-restricted)",
                background: "color-mix(in srgb, currentColor 12%, transparent)",
              }}
            >
              {v ? "✓" : "✕"} {label}
            </span>
          ))}
        </div>
      )}
    </div>
  );
}

interface LicenseDraft {
  id: string;
  commercial: TriState;
  modify: TriState;
  redistribute: TriState;
  attribution: TriState;
  holder: string;
  credit: string;
  url: string;
}

/** Per-asset licence editor. A single-asset edit owns the whole block, so it sends every field
 *  explicitly — the three-state patch earns its keep in the *bulk* editor, where "leave this alone"
 *  is a real answer. Each right is three-state here too: conflating "unknown" with "no" is the exact
 *  bug the nullable rights columns exist to prevent. */
function LicenseEditor({
  asset,
  onDone,
  onCancel,
}: {
  asset: Asset;
  onDone: (result: LicenseEditResult) => void;
  onCancel: () => void;
}) {
  const setLicense = useSetLicense();
  const { license } = asset;
  const [draft, setDraft] = useState<LicenseDraft>({
    id: license.id ?? "",
    commercial: triFromBool(license.commercial),
    modify: triFromBool(license.modify),
    redistribute: triFromBool(license.redistribute),
    attribution: triFromBool(license.attribution),
    holder: license.holder ?? "",
    credit: license.credit ?? "",
    url: license.url ?? "",
  });
  const [presetNote, setPresetNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const field = <K extends keyof LicenseDraft>(key: K, value: LicenseDraft[K]) =>
    setDraft((prev) => ({ ...prev, [key]: value }));

  const applyPreset = (id: string) => {
    const preset = LICENSE_PRESETS.find((candidate) => candidate.id === id);
    if (!preset) return;
    setPresetNote(
      preset.note ??
        "Pre-filled from a well-known licence — review it against the actual grant, then save.",
    );
    setDraft((prev) => ({
      ...prev,
      id: preset.id,
      ...(preset.rights
        ? {
            commercial: triFromBool(preset.rights.commercial),
            modify: triFromBool(preset.rights.modify),
            redistribute: triFromBool(preset.rights.redistribute),
            attribution: triFromBool(preset.rights.attribution),
          }
        : {}),
    }));
  };

  const save = () => {
    setError(null);
    const patch: LicenseInput = {
      id: textPatch(draft.id),
      commercial: triToBool(draft.commercial),
      modify: triToBool(draft.modify),
      redistribute: triToBool(draft.redistribute),
      attribution: triToBool(draft.attribution),
      holder: textPatch(draft.holder),
      credit: textPatch(draft.credit),
      url: textPatch(draft.url),
    };
    setLicense.mutate(
      { assets: [asset.summary.id], license: patch },
      {
        onSuccess: onDone,
        onError: (reason) =>
          setError(reason instanceof ApiError ? reason.message : String(reason)),
      },
    );
  };

  return (
    <div className="mt-2 space-y-2 rounded border border-border bg-surface-2 p-2">
      <p className="text-[10px] text-fg-dim">
        The badge is derived from the identifier plus the four rights — it is never something you set
        directly. “Unknown” stays unknown until someone establishes it.
      </p>

      <label className="block text-[11px] text-fg-muted">
        Identifier
        <input
          className="field mt-1"
          value={draft.id}
          list="license-preset-ids"
          placeholder="e.g. CC-BY-4.0, Proprietary"
          onChange={(event) => field("id", event.target.value)}
        />
      </label>
      <datalist id="license-preset-ids">
        {LICENSE_PRESETS.map((preset) => (
          <option key={preset.id} value={preset.id}>
            {preset.label}
          </option>
        ))}
      </datalist>

      <label className="block text-[11px] text-fg-muted">
        Pre-fill from a known licence
        <select
          className="field mt-1"
          value=""
          aria-label="Pre-fill from a known licence"
          onChange={(event) => {
            if (event.target.value) applyPreset(event.target.value);
            event.currentTarget.value = "";
          }}
        >
          <option value="">Choose a licence…</option>
          {LICENSE_PRESETS.map((preset) => (
            <option key={preset.id} value={preset.id}>
              {preset.label}
            </option>
          ))}
        </select>
      </label>
      {presetNote && <p className="text-[10px] text-warn">{presetNote}</p>}

      {RIGHTS_FIELDS.map(([key, label, hint]) => (
        <label key={key} className="block text-[11px] text-fg-muted" title={hint}>
          {label}
          <select
            className="field mt-1"
            value={draft[key]}
            aria-label={label}
            onChange={(event) => field(key, event.target.value as TriState)}
          >
            {TRI_OPTIONS.map(([value, optionLabel]) => (
              <option key={value} value={value}>
                {optionLabel}
              </option>
            ))}
          </select>
        </label>
      ))}

      <label className="block text-[11px] text-fg-muted">
        Rights holder
        <input
          className="field mt-1"
          value={draft.holder}
          placeholder="Who owns it"
          onChange={(event) => field("holder", event.target.value)}
        />
      </label>
      <label className="block text-[11px] text-fg-muted">
        Credit line
        <input
          className="field mt-1"
          value={draft.credit}
          placeholder="How to credit them"
          onChange={(event) => field("credit", event.target.value)}
        />
      </label>
      <label className="block text-[11px] text-fg-muted">
        Source URL
        <input
          className="field mt-1"
          type="url"
          value={draft.url}
          placeholder="https://…"
          onChange={(event) => field("url", event.target.value)}
        />
      </label>

      {error && (
        <p role="alert" className="text-[11px] text-danger">
          {error}
        </p>
      )}
      <div className="flex justify-end gap-2">
        <button className="btn" onClick={onCancel} disabled={setLicense.isPending}>
          Cancel
        </button>
        <button className="btn btn-accent" onClick={save} disabled={setLicense.isPending}>
          {setLicense.isPending ? "Saving…" : "Save licence"}
        </button>
      </div>
    </div>
  );
}
