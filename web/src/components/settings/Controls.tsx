// The two administration form primitives. They are shared by the flag cards and by the
// account/group rows, so they live beside the sections rather than inside any one of them.

export function Choice({
  value,
  options,
  onChange,
  disabled,
  label,
}: {
  value: string;
  options: string[];
  onChange: (v: string) => void;
  disabled?: boolean;
  /** Accessible name — the flag's title (a11y: axe select-name, issue #44). */
  label: string;
}) {
  return (
    <select
      className="field w-auto disabled:opacity-50"
      aria-label={label}
      value={value}
      disabled={disabled}
      onChange={(e) => onChange(e.target.value)}
    >
      {options.map((o) => (
        <option key={o} value={o}>
          {o}
        </option>
      ))}
    </select>
  );
}

export function Toggle({
  checked,
  onChange,
  disabled,
  label,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
  /** Accessible name — the flag's title (a11y: axe button-name, issue #44). */
  label: string;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className={`h-6 w-11 self-end rounded-full transition disabled:opacity-50 coarse:h-11 coarse:w-14 ${
        checked ? "bg-accent" : "bg-surface-2"
      }`}
    >
      <span
        className={`block h-5 w-5 rounded-full bg-fg transition coarse:h-6 coarse:w-6 ${
          checked ? "translate-x-5 coarse:translate-x-7" : "translate-x-0.5 coarse:translate-x-1"
        }`}
      />
    </button>
  );
}
