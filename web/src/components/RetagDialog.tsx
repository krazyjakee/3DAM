import { useMemo, useState } from "react";
import { Tags } from "lucide-react";
import { useEditTags, useTagVocabulary } from "@/api/queries";
import type { QueryRequest, TagEditRequest, TagEditResult } from "@/api/types";
import { ApiError } from "@/api/client";
import { Modal } from "@/lib/dialogs";

export type RetagScope =
  | { assets: string[] }
  | { collection: string }
  | { query: QueryRequest };

function tags(value: string): string[] {
  return [...new Set(value.split(",").map((tag) => tag.trim().toLowerCase()).filter(Boolean))];
}

export function RetagDialog({
  scope,
  excludedPeers = 0,
  onClose,
}: {
  scope: RetagScope;
  excludedPeers?: number;
  onClose: () => void;
}) {
  const edit = useEditTags();
  const [addText, setAddText] = useState("");
  const [removeText, setRemoveText] = useState("");
  const [preview, setPreview] = useState<TagEditResult | null>(null);
  const [applied, setApplied] = useState<TagEditResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const vocabulary = useTagVocabulary(addText.trim());
  const add = useMemo(() => tags(addText), [addText]);
  const remove = useMemo(() => tags(removeText), [removeText]);
  const overlap = add.find((tag) => remove.includes(tag));
  const request = (dryRun: boolean): TagEditRequest => ({
    ...scope,
    add,
    remove,
    dry_run: dryRun,
  });
  const change = (setter: (value: string) => void, value: string) => {
    setter(value);
    setPreview(null);
    setApplied(null);
    setError(null);
  };
  const submit = (dryRun: boolean) => {
    setError(null);
    edit.mutate(request(dryRun), {
      onSuccess: (result) => {
        if (dryRun) setPreview(result);
        else setApplied(result);
      },
      onError: (reason) => setError(reason instanceof ApiError ? reason.message : String(reason)),
    });
  };
  const result = applied ?? preview;

  return (
    <Modal title="Retag selection" icon={<Tags size={15} />} labelledBy="retag-title" onClose={onClose}>
      <div className="flex flex-col gap-3">
        <p className="text-[11px] text-fg-dim">
          Manual tags are separate from automatic suggestions. Preview uses the same permissions and
          target resolution as apply.
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
        <label className="text-[11px] text-fg-muted">
          Add manual tags
          <input
            className="field mt-1"
            value={addText}
            onChange={(event) => change(setAddText, event.target.value)}
            placeholder="favorite, approved"
            list="retag-vocabulary"
          />
        </label>
        <label className="text-[11px] text-fg-muted">
          Remove manual tags
          <input
            className="field mt-1"
            value={removeText}
            onChange={(event) => change(setRemoveText, event.target.value)}
            placeholder="old-tag, needs-review"
            list="retag-vocabulary"
          />
        </label>
        <datalist id="retag-vocabulary">
          {(vocabulary.data ?? []).map((tag) => <option key={tag.name} value={tag.name} />)}
        </datalist>
        {overlap && <p className="text-[11px] text-danger">“{overlap}” cannot be both added and removed.</p>}
        {result && (
          <div className="rounded border border-border bg-surface-2 p-2 text-[11px] text-fg-muted">
            <p>
              {applied ? "Applied" : "Preview"}: {result.changed} of {result.matched} assets change · +
              {result.additions} / −{result.removals} tag assignments
            </p>
            {result.warnings.map((warning, index) => (
              <p key={`${warning.code}:${warning.subject}:${index}`} className="mt-1 text-fg-dim">
                {warning.message}
              </p>
            ))}
          </div>
        )}
        {error && <p className="text-[11px] text-danger">{error}</p>}
        <div className="flex justify-end gap-2">
          <button className="btn" onClick={onClose}>{applied ? "Done" : "Cancel"}</button>
          {!applied && (
            <>
              <button
                className="btn"
                disabled={edit.isPending || (add.length === 0 && remove.length === 0) || !!overlap}
                onClick={() => submit(true)}
              >
                {edit.isPending ? "Checking…" : "Preview"}
              </button>
              <button
                className="btn btn-accent"
                disabled={edit.isPending || !preview || !!overlap}
                onClick={() => submit(false)}
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
