// Shared licence-editing vocabulary (issue #106): the tri-state the four rights columns actually
// have, and a short table of SPDX presets.
//
// Two invariants this module exists to protect:
//
// 1. **Unknown is a value, not a default `false`.** `rights_commercial` and friends are nullable on
//    purpose — "nobody has established whether you may sell this" is a different, and much more
//    dangerous, statement than "you may not". Every control over them is therefore three-state.
// 2. **The server never infers rights from an identifier** (ADR 0009 §1: "No defaults. Unknown is
//    unknown."). Presets are a *client-side* convenience: picking one pre-fills the four rights into
//    the visible form so the user reviews and saves them deliberately. Nothing here is sent as an
//    id-only hint that the backend would expand.

import type { LicenseInput, Patch } from "@/api/types";

/** How a nullable rights column presents in a form control. */
export type TriState = "unknown" | "yes" | "no";

export function triFromBool(value: boolean | null | undefined): TriState {
  return value == null ? "unknown" : value ? "yes" : "no";
}

/** `null` here is the wire's "clear this column back to unknown", not "absent". */
export function triToBool(value: TriState): boolean | null {
  return value === "unknown" ? null : value === "yes";
}

export const TRI_OPTIONS: [TriState, string][] = [
  ["unknown", "Unknown"],
  ["yes", "Yes"],
  ["no", "No"],
];

/** The four rights columns, in the order `derive_license_status` reads them. */
export const RIGHTS_FIELDS = [
  ["commercial", "Commercial use", "May this be used in something sold?"],
  ["modify", "Modify", "May this be edited or derived from?"],
  ["redistribute", "Redistribute", "May this be shipped on to others?"],
  ["attribution", "Attribution required", "Must the author be credited? “Yes” means required."],
] as const;

export type RightKey = (typeof RIGHTS_FIELDS)[number][0];

export interface RightsSet {
  commercial: boolean;
  modify: boolean;
  redistribute: boolean;
  /** `true` means attribution is *required*. */
  attribution: boolean;
}

export interface LicensePreset {
  id: string;
  label: string;
  /** `null` fills the identifier only and leaves the rights for the user to establish — the honest
   *  answer for a sentinel whose terms are per-asset. */
  rights: RightsSet | null;
  note?: string;
}

const FREE = { commercial: true, modify: true, redistribute: true } as const;

/** Deliberately short. A wrong rights mapping is worse than no preset, so this covers only licences
 *  whose four answers are unambiguous under this model. Share-alike / copyleft obligations are a
 *  *condition on derivatives* that the four columns cannot express, so those licences are absent
 *  rather than approximated. */
export const LICENSE_PRESETS: LicensePreset[] = [
  { id: "CC0-1.0", label: "CC0 1.0 (public domain)", rights: { ...FREE, attribution: false } },
  { id: "Unlicense", label: "The Unlicense", rights: { ...FREE, attribution: false } },
  { id: "CC-BY-4.0", label: "CC BY 4.0", rights: { ...FREE, attribution: true } },
  {
    id: "CC-BY-NC-4.0",
    label: "CC BY-NC 4.0 (non-commercial)",
    rights: { commercial: false, modify: true, redistribute: true, attribution: true },
  },
  {
    id: "CC-BY-ND-4.0",
    label: "CC BY-ND 4.0 (no derivatives)",
    rights: { commercial: true, modify: false, redistribute: true, attribution: true },
  },
  { id: "MIT", label: "MIT", rights: { ...FREE, attribution: true } },
  { id: "Apache-2.0", label: "Apache 2.0", rights: { ...FREE, attribution: true } },
  {
    id: "Proprietary",
    label: "Proprietary (fill in the rights yourself)",
    rights: null,
    note: "Terms vary per purchase — establish the four rights from the licence you were granted.",
  },
  {
    id: "Custom",
    label: "Custom (fill in the rights yourself)",
    rights: null,
    note: "Record what the agreement actually says; nothing is inferred from the name.",
  },
];

/** Trim a text field into its patch value: an emptied box means "clear it", not "leave it". */
export function textPatch(value: string): Patch<string> {
  const trimmed = value.trim();
  return trimmed === "" ? null : trimmed;
}

/** Whether a patch would touch any column at all. Mirrors `LicenseInput::is_empty`. */
export function isEmptyPatch(patch: LicenseInput): boolean {
  return Object.values(patch).every((value) => value === undefined);
}

// ── usage-right filters (AdvancedSearch) ────────────────────────────────────

/** The `usage_right` facet's accepted values, and how each one reads in both directions.
 *
 *  **The op is load-bearing** (dam-store `helpers.rs`, `UsageRight`). The facet names *which* right;
 *  the op says which way it must be settled:
 *
 *  - `eq` → `rights_<name> = 1` — the right is granted (or, for attribution, required).
 *  - `ne` → `rights_<name> = 0` — the right is **known** to be denied (or known not required).
 *  - anything else is rejected by the store as a bad request.
 *
 *  `ne` is `= 0`, deliberately not `<> 1`: `<> 1` would also match `NULL`, sweeping every asset whose
 *  terms nobody has established into a result the user reads as cleared to ship. So an *unknown*
 *  right matches neither direction, and there is no filter that means "unknown or not required".
 *  That is why the controls over this facet are three-state and why every surface that renders one
 *  has to render the op — `commercial eq` and `commercial ne` are opposite claims about a safety
 *  field, and a chip that showed them identically would be worse than no chip.
 *
 *  All the wording lives here rather than in each renderer, because getting one direction right and
 *  the other backwards is the failure mode, and it is easiest to spot when both sit side by side.
 *  Attribution is the odd one out: the column means *required*, so its `eq` is an obligation rather
 *  than a permission and its `ne` reads "no attribution required", never "no attribution". */
export const USAGE_RIGHTS = [
  {
    key: "commercial",
    label: "Commercial use",
    /** Option labels for the three-state control. */
    granted: "Allowed",
    denied: "Denied",
    /** Filter-chip phrasing (terse) and saved-search phrasing (a sentence). */
    chip: { eq: "Allows commercial use", ne: "Commercial use denied" },
    summary: { eq: "Commercial use is allowed", ne: "Commercial use is denied" },
  },
  {
    key: "modify",
    label: "Modification",
    granted: "Allowed",
    denied: "Denied",
    chip: { eq: "Allows modification", ne: "Modification denied" },
    summary: { eq: "Modification is allowed", ne: "Modification is denied" },
  },
  {
    key: "redistribute",
    label: "Redistribution",
    granted: "Allowed",
    denied: "Denied",
    chip: { eq: "Allows redistribution", ne: "Redistribution denied" },
    summary: { eq: "Redistribution is allowed", ne: "Redistribution is denied" },
  },
  {
    key: "attribution",
    label: "Attribution",
    granted: "Required",
    denied: "Not required",
    chip: { eq: "Requires attribution", ne: "No attribution required" },
    summary: { eq: "Attribution is required", ne: "No attribution required" },
  },
] as const;

export type UsageRight = (typeof USAGE_RIGHTS)[number]["key"];

/** The only two ops the `usage_right` facet accepts. */
export type UsageRightOp = "eq" | "ne";

export function usageRightPhrase(
  right: string,
  op: string,
  style: "chip" | "summary",
): string | null {
  const spec = USAGE_RIGHTS.find((candidate) => candidate.key === right);
  if (!spec || (op !== "eq" && op !== "ne")) return null;
  return spec[style][op];
}
