/** Compact label/value pair used by administration status and storage summaries. */
export function AdminField({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex flex-col">
      <span className="text-xs text-fg-dim">{label}</span>
      <span className="font-mono">{value}</span>
    </div>
  );
}
