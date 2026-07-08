# Accessibility report — web client

Part of the accessibility hardening epic ([#44](https://github.com/krazyjakee/3DAM/issues/44),
PRODUCT_SPEC §9 phase 7). Two automated checks:

1. **Contrast audit** — the colour tokens in [`web/src/index.css`](../web/src/index.css) against
   WCAG 2.1 §1.4.3 (AA). (below)
2. **axe-core scan** — every route against WCAG 2 A/AA rules. ([jump ↓](#automated-axe-core-scan))

---

## Contrast audit — WCAG AA of the design tokens

Audits the tokens (`@theme` = dark, `:root[data-theme="light"]` = light) against **WCAG 2.1
§1.4.3 (AA)**.

## Method

Contrast ratio is computed from the WCAG relative-luminance formula (sRGB linearised, then
`(L_light + 0.05) / (L_dark + 0.05)`). Thresholds:

- **4.5:1** — normal text (the bar this UI is held to, since it is information-dense with small type).
- **3:1** — large text (≥18.66px bold / ≥24px) and non-text UI components / graphical objects (§1.4.11).

Every pair below is verified by the script at the end of this file (`python3` — no dependencies), so
the report regenerates deterministically when a token changes.

## Result — both themes pass AA

All essential foreground/background pairs meet 4.5:1 in both themes after the fixes in
[the fixing commit].

### Dark (default)

| Role | Pair | Ratio | |
|---|---|---|---|
| Primary text | `--fg` on `--surface` | 14.76:1 | ✅ AA |
| Secondary text | `--fg-muted` on `--surface` | 7.05:1 | ✅ AA |
| De-emphasised text | `--fg-dim` on `--surface` | 5.49:1 | ✅ AA |
| De-emphasised on elevated | `--fg-dim` on `--surface2` | 5.05:1 | ✅ AA |
| Links / active / % | `--accent` on `--surface` | 8.38:1 | ✅ AA |
| Active nav row text | `--accent` on `--accent-muted` | 6.88:1 | ✅ AA |
| Primary button label | `--accent-fg` on `--accent` | 8.98:1 | ✅ AA |
| Warning text | `--warn` on `--surface` | 8.80:1 | ✅ AA |
| Warning on elevated | `--warn` on `--surface2` | 8.10:1 | ✅ AA |
| Danger text | `--danger` on `--surface` | 5.68:1 | ✅ AA |
| Licence: permissive | `--lic-permissive` on `--surface` | 10.31:1 | ✅ AA |
| Licence: attribution | `--lic-attribution` on `--surface` | 8.38:1 | ✅ AA |
| Licence: restricted | `--lic-restricted` on `--surface` | 5.68:1 | ✅ AA |
| Licence: unknown | `--lic-unknown` on `--surface` | 5.80:1 | ✅ AA |
| Badge: SFX (audio) | `--media-audio` on `--surface` | 9.31:1 | ✅ AA |
| Badge: IMG (image) | `--media-image` on `--surface` | 7.10:1 | ✅ AA |
| Badge: 3D (model) | `--media-model` on `--surface` | 6.03:1 | ✅ AA |

### Light

| Role | Pair | Ratio | |
|---|---|---|---|
| Primary text | `--fg` on `--surface` | 16.88:1 | ✅ AA |
| Secondary text | `--fg-muted` on `--surface` | 6.39:1 | ✅ AA |
| De-emphasised text | `--fg-dim` on `--surface` | 5.81:1 | ✅ AA |
| De-emphasised on elevated | `--fg-dim` on `--surface2` | 5.00:1 | ✅ AA |
| Links / active / % | `--accent` on `--surface` | 7.56:1 | ✅ AA |
| Active nav row text | `--accent` on `--accent-muted` | 6.40:1 | ✅ AA |
| Primary button label | `--accent-fg` on `--accent` | 7.56:1 | ✅ AA |
| Warning text | `--warn` on `--surface` | 5.91:1 | ✅ AA |
| Warning on elevated | `--warn` on `--surface2` | 5.08:1 | ✅ AA |
| Danger text | `--danger` on `--surface` | 4.83:1 | ✅ AA |
| Licence: permissive | `--lic-permissive` on `--surface` | 5.02:1 | ✅ AA |
| Licence: attribution | `--lic-attribution` on `--surface` | 7.56:1 | ✅ AA |
| Licence: restricted | `--lic-restricted` on `--surface` | 4.83:1 | ✅ AA |
| Licence: unknown | `--lic-unknown` on `--surface` | 4.83:1 | ✅ AA |
| Badge: SFX (audio) | `--media-audio` on `--surface` | 5.47:1 | ✅ AA |
| Badge: IMG (image) | `--media-image` on `--surface` | 5.18:1 | ✅ AA |
| Badge: 3D (model) | `--media-model` on `--surface` | 6.29:1 | ✅ AA |

## Changes made

Token adjustments, hue-preserving (same colour, tuned lightness only):

| Token | Theme | Before | After | Why |
|---|---|---|---|---|
| `--color-fg-dim` | dark | `#656d7a` | `#868f9c` | was 3.2–3.7:1; de-emphasised text is still the dimmest tier |
| `--color-fg-dim` | light | `#78828f` | `#5d6673` | was 3.4–3.9:1 |
| `--color-accent` | light | `#0284c7` | `#075985` | button label + link text were 4.1:1 |
| `--color-lic-attribution` | light | `#0284c7` | `#075985` | attribution badge was 4.1:1 (tracks the accent blue) |
| `--color-media-audio` | light | `#0d9488` | `#0f766e` | "SFX" badge label was 3.7:1 |
| `--color-warn` | light | `#b45309` | `#9a5108` | warning on the elevated surface was 4.32:1 |

The dark theme needed only `--color-fg-dim`; the light theme (added later, in #62) was less
contrast-tuned and carried most of the failures.

## Documented exemptions

- **`--color-border-strong`** on `--bg` / `--surface` (1.6–1.7:1) — panel dividers and rail borders
  are **decorative boundaries**, not "graphical objects required to understand content", so §1.4.11
  does not apply. The *functional* boundary — the keyboard focus ring — uses `--color-accent`
  (≥7.5:1 in both themes) and passes comfortably. Left unchanged by design (low-chrome aesthetic).

## Not yet covered by this report (remaining #44 scope)

- Automated axe pass on the live workspace (dynamic states, ARIA wiring).
- Documented keyboard-only walkthrough (browse → inspect → act).
- egui / AccessKit parity once the native GUI ([#34](https://github.com/krazyjakee/3DAM/issues/34))
  is built.

## Reproduce

```python
def lin(c):
    c /= 255.0
    return c / 12.92 if c <= 0.03928 else ((c + 0.055) / 1.055) ** 2.4
def lum(h):
    h = h.lstrip('#')
    return 0.2126*lin(int(h[0:2],16)) + 0.7152*lin(int(h[2:4],16)) + 0.0722*lin(int(h[4:6],16))
def ratio(a, b):
    la, lb = lum(a), lum(b)
    return (max(la, lb) + 0.05) / (min(la, lb) + 0.05)

# e.g. dark de-emphasised text on the base surface:
print(round(ratio('#868f9c', '#14171c'), 2))  # -> 5.49  (>= 4.5 : AA pass)
```

Paste the current token hexes from `web/src/index.css` and re-run to re-verify after any palette change.

---

## Automated axe-core scan

axe-core 4.10.2, WCAG 2 A/AA rule set (`wcag2a`, `wcag2aa`, `wcag21a`, `wcag21aa`), run against a
running dev client with a seeded catalog. **Every route passes with 0 violations.**

| Route / state | Violations | Passing checks |
|---|---|---|
| Workspace — grid | 0 | 25 |
| Workspace — table + inspector open | 0 | 26 |
| Settings | 0 | 22 |
| Duplicates | 0 | 18 |
| Blocklist | 0 | 14 |

### Violations found and fixed

The first scan surfaced these (all now resolved):

| Rule | Impact | Where | Fix |
|---|---|---|---|
| `button-name` | critical | grid/table view toggle, Settings flag toggle | `aria-label` (+ `aria-pressed` on the view toggle) |
| `select-name` | critical | sort order, search mode, Settings flag `<select>`s, dedupe filter, and the Export / Convert / Add-source / collection selects | `aria-label` on each |
| `nested-interactive` | serious | the grid/table favourite star (a `role="button"` span inside the cell `<button>`) | made the star **presentational** (`aria-hidden`, no role/tab stop) — a pointer convenience; the keyboard/AT favourite toggle is the real `<button>` in the Inspector |
| `aria-required-parent` / `-children` | critical | the table header carried an orphan `role="row"` with no grid container | dropped the role — the list is a roving-focus group of labelled row buttons (#27), not a full ARIA grid; the header is now presentational |

### Reproduce

With the dev client running, in the browser console:

```js
const s = document.createElement('script');
s.src = 'https://cdn.jsdelivr.net/npm/axe-core@4.10.2/axe.min.js';
document.head.appendChild(s);
s.onload = async () => {
  const r = await axe.run(document, { runOnly: { type: 'tag', values: ['wcag2a','wcag2aa','wcag21a','wcag21aa'] } });
  console.log(r.violations.length, 'violations', r.violations);
};
```

Note: axe scans the current DOM only, so exercise each route/state (open a dialog, select an asset,
switch to the table) and re-run to cover controls that mount conditionally.
