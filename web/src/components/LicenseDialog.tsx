import { useMemo, useState } from "react";
import { ScrollText } from "lucide-react";
import { useSetLicense } from "@/api/queries";
import type {
  LicenseEditResult,
  LicenseInput,
  QueryRequest,
  SetLicenseRequest,
} from "@/api/types";
import { ApiError } from "@/api/client";
import { Modal } from "@/lib/dialogs";
import { LicenseBadge } from "./LicenseBadge";
import { isEmptyPatch, LICENSE_PRESETS, RIGHTS_FIELDS, type RightKey } from "@/lib/license";

/** Same precedence as the retag flow: explicit ids, else a collection, else the live query. */
export type LicenseScope = { assets: string[] } | { collection: string } | { query: QueryRequest };

/** A bulk field's editing mode. `leave` is the default and the reason the wire patch is three-state:
 *  over a mixed selection "don't touch this" is a real, common answer that is *not* the same as
 *  "make it unknown". */
type Mode = "leave" | "set" | "clear";

type TriMode = "leave" | "yes" | "no" | "clear";

const TRI_MODES: [TriMode, string][] = [
  ["leave", "Leave unchanged"],
  ["yes", "Yes"],
  ["no", "No"],
  ["clear", "Back to unknown"],
];

function triPatch(mode: TriMode): boolean | null | undefined {
  if (mode === "leave") return undefined;
  if (mode === "clear") return null;
  return mode === "yes";
}

function textFieldPatch(mode: Mode, value: string): string | null | undefined {
  if (mode === "leave") return undefined;
  if (mode === "clear") return null;
  const trimmed = value.trim();
  return trimmed === "" ? null : trimmed;
}

function PatchText({
  label,
  placeholder,
  mode,
  value,
  list,
  onMode,
  onValue,
}: {
  label: string;
  placeholder?: string;
  mode: Mode;
  value: string;
  list?: string;
  onMode: (mode: Mode) => void;
  onValue: (value: string) => void;
}) {
  return (
    <div className="flex flex-col gap-1">
      <span className="text-[11px] text-fg-muted">{label}</span>
      <div className="flex gap-2">
        <select
          className="field w-40 shrink-0"
          aria-label={`${label} action`}
          value={mode}
          onChange={(event) => onMode(event.target.value as Mode)}
        >
          <option value="leave">Leave unchanged</option>
          <option value="set">Set to…</option>
          <option value="clear">Clear</option>
        </select>
        <input
          className="field min-w-0 flex-1"
          aria-label={label}
          list={list}
          disabled={mode !== "set"}
          placeholder={placeholder}
          value={value}
          onChange={(event) => onValue(event.target.value)}
        />
      </div>
    </div>
  );
}

/** Bulk licence editing with a mandatory preview (DESIGN_GUIDELINES §3.4: bulk operations preview
 *  their effect). The preview is a real `dry_run` request — same permissions, same target
 *  resolution, same derivation — so what it reports is what applying will do. Applying then reports
 *  the per-target warnings that a summary-shaped result carries instead of a row per asset. */
export function LicenseDialog({
  scope,
  excludedPeers = 0,
  onClose,
}: {
  scope: LicenseScope;
  excludedPeers?: number;
  onClose: () => void;
}) {
  const edit = useSetLicense();
  const [idMode, setIdMode] = useState<Mode>("leave");
  const [idValue, setIdValue] = useState("");
  const [holderMode, setHolderMode] = useState<Mode>("leave");
  const [holderValue, setHolderValue] = useState("");
  const [creditMode, setCreditMode] = useState<Mode>("leave");
  const [creditValue, setCreditValue] = useState("");
  const [urlMode, setUrlMode] = useState<Mode>("leave");
  const [urlValue, setUrlValue] = useState("");
  const [rights, setRights] = useState<Record<RightKey, TriMode>>({
    commercial: "leave",
    modify: "leave",
    redistribute: "leave",
    attribution: "leave",
  });
  const [preview, setPreview] = useState<LicenseEditResult | null>(null);
  const [applied, setApplied] = useState<LicenseEditResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  /** Only the fields the user actually touched reach the wire — everything else stays absent. */
  const patch: LicenseInput = useMemo(() => {
    const built: LicenseInput = {
      id: textFieldPatch(idMode, idValue),
      commercial: triPatch(rights.commercial),
      modify: triPatch(rights.modify),
      redistribute: triPatch(rights.redistribute),
      attribution: triPatch(rights.attribution),
      holder: textFieldPatch(holderMode, holderValue),
      credit: textFieldPatch(creditMode, creditValue),
      url: textFieldPatch(urlMode, urlValue),
    };
    // `JSON.stringify` drops `undefined` values, so an untouched field never appears on the wire.
    return built;
  }, [
    idMode,
    idValue,
    holderMode,
    holderValue,
    creditMode,
    creditValue,
    urlMode,
    urlValue,
    rights,
  ]);

  const empty = isEmptyPatch(patch);
  const invalidated = () => {
    setPreview(null);
    setApplied(null);
    setError(null);
  };

  const submit = (dryRun: boolean) => {
    setError(null);
    const request: SetLicenseRequest = { ...scope, license: patch, dry_run: dryRun };
    edit.mutate(request, {
      onSuccess: (result) => (dryRun ? setPreview(result) : setApplied(result)),
      onError: (reason) => setError(reason instanceof ApiError ? reason.message : String(reason)),
    });
  };

  const result = applied ?? preview;

  return (
    <Modal
      title="Set licence for selection"
      icon={<ScrollText size={15} />}
      labelledBy="license-dialog-title"
      onClose={onClose}
      wide
      scroll
    >
      <div className="flex flex-col gap-3">
        <p className="text-[11px] text-fg-dim">
          Every field defaults to “leave unchanged”, so a bulk edit only touches what you set. The
          licence badge is derived from the identifier plus the four rights — “permissive” is
          unreachable without a named licence and all four rights known.
        </p>
        {excludedPeers > 0 && (
          <p className="text-[11px] text-fg-dim">
            {excludedPeers} federated target{excludedPeers === 1 ? " is" : "s are"} read-only and
            excluded.
          </p>
        )}
        {!("assets" in scope) && (
          <p className="text-[11px] text-fg-dim">
            Query and collection selections are resolved against this local catalog; peer results are
            never written.
          </p>
        )}

        <div onChangeCapture={invalidated} className="flex flex-col gap-3">
          <PatchText
            label="Licence identifier"
            placeholder="e.g. CC-BY-4.0"
            list="license-dialog-ids"
            mode={idMode}
            value={idValue}
            onMode={setIdMode}
            onValue={setIdValue}
          />
          <datalist id="license-dialog-ids">
            {LICENSE_PRESETS.map((preset) => (
              <option key={preset.id} value={preset.id}>
                {preset.label}
              </option>
            ))}
          </datalist>

          <fieldset className="grid grid-cols-1 gap-2 sm:grid-cols-2">
            <legend className="mb-1 text-[11px] text-fg-muted">Usage rights</legend>
            {RIGHTS_FIELDS.map(([key, label, hint]) => (
              <label key={key} className="text-[11px] text-fg-muted" title={hint}>
                {label}
                <select
                  className="field mt-1"
                  aria-label={label}
                  value={rights[key]}
                  onChange={(event) =>
                    setRights((prev) => ({ ...prev, [key]: event.target.value as TriMode }))
                  }
                >
                  {TRI_MODES.map(([value, optionLabel]) => (
                    <option key={value} value={value}>
                      {optionLabel}
                    </option>
                  ))}
                </select>
              </label>
            ))}
          </fieldset>

          <PatchText
            label="Rights holder"
            mode={holderMode}
            value={holderValue}
            onMode={setHolderMode}
            onValue={setHolderValue}
          />
          <PatchText
            label="Credit line"
            mode={creditMode}
            value={creditValue}
            onMode={setCreditMode}
            onValue={setCreditValue}
          />
          <PatchText
            label="Source URL"
            placeholder="https://…"
            mode={urlMode}
            value={urlValue}
            onMode={setUrlMode}
            onValue={setUrlValue}
          />
        </div>

        {result && (
          <div className="rounded border border-border bg-surface-2 p-2 text-[11px] text-fg-muted">
            <p>
              {applied ? "Applied" : "Preview"}: {result.changed.toLocaleString()} of{" "}
              {result.matched.toLocaleString()} assets change
            </p>
            {result.status.length > 0 && (
              <div className="mt-1 flex flex-wrap items-center gap-2">
                <span className="text-fg-dim">Resulting status:</span>
                {result.status.map((entry) => (
                  <span key={entry.status} className="inline-flex items-center gap-1">
                    <LicenseBadge badge={{ id: null, status: entry.status }} />
                    <span className="tabular-nums">×{entry.count.toLocaleString()}</span>
                  </span>
                ))}
              </div>
            )}
            {result.warnings.map((warning, index) => (
              <p key={`${warning.code}:${warning.subject}:${index}`} className="mt-1 text-warn">
                {warning.message}
              </p>
            ))}
            {applied && result.warnings.length === 0 && (
              <p className="mt-1 text-fg-dim">No failures reported.</p>
            )}
          </div>
        )}
        {error && (
          <p role="alert" className="text-[11px] text-danger">
            {error}
          </p>
        )}

        <div className="flex justify-end gap-2">
          <button className="btn" onClick={onClose}>
            {applied ? "Done" : "Cancel"}
          </button>
          {!applied && (
            <>
              <button className="btn" disabled={edit.isPending || empty} onClick={() => submit(true)}>
                {edit.isPending ? "Checking…" : "Preview"}
              </button>
              <button
                className="btn btn-accent"
                disabled={edit.isPending || !preview}
                onClick={() => submit(false)}
                title={preview ? "Apply the previewed change" : "Preview the change first"}
              >
                Apply
              </button>
            </>
          )}
        </div>
      </div>
    </Modal>
  );
}
