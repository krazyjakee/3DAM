//! Web-matched egui theme (issue #68).
//!
//! Builds concrete [`egui::Visuals`] from the web client's Tailwind `@theme` design tokens
//! (`web/src/index.css`, governed by `docs/DESIGN_GUIDELINES.md`) so the native GUI and the web app
//! read as the same product: dark-first, low-chrome, information-dense, one sky accent (`#38bdf8`),
//! with warn/danger reserved for exposure risk. The rail Theme control picks a preference
//! (System/Dark/Light); this module decides the concrete colours each resolves to.

use eframe::egui::{self, Color32, Stroke};

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// One mode's design tokens (names mirror the web `--color-*` custom properties).
struct Palette {
    bg: Color32,
    surface: Color32,
    surface2: Color32,
    border: Color32,
    border_strong: Color32,
    fg: Color32,
    fg_muted: Color32,
    accent: Color32,
    accent_fg: Color32,
    accent_muted: Color32,
    warn: Color32,
    danger: Color32,
}

// Dark palette — the `@theme` block in web/src/index.css (dark-first, DESIGN_GUIDELINES §4).
const DARK: Palette = Palette {
    bg: rgb(0x0b, 0x0d, 0x10),
    surface: rgb(0x14, 0x17, 0x1c),
    surface2: rgb(0x1b, 0x1f, 0x26),
    border: rgb(0x26, 0x2b, 0x33),
    border_strong: rgb(0x33, 0x3a, 0x45),
    fg: rgb(0xe6, 0xe9, 0xee),
    fg_muted: rgb(0x9a, 0xa3, 0xb0),
    accent: rgb(0x38, 0xbd, 0xf8),
    accent_fg: rgb(0x05, 0x10, 0x16),
    accent_muted: rgb(0x0e, 0x2b, 0x3a),
    warn: rgb(0xf5, 0xa5, 0x24),
    danger: rgb(0xf0, 0x61, 0x6d),
};

// Light palette — the `.light`/`:root[data-theme=light]` overrides in web/src/index.css.
const LIGHT: Palette = Palette {
    bg: rgb(0xf6, 0xf7, 0xf9),
    surface: rgb(0xff, 0xff, 0xff),
    surface2: rgb(0xec, 0xee, 0xf1),
    border: rgb(0xda, 0xdf, 0xe6),
    border_strong: rgb(0xc2, 0xc9, 0xd3),
    fg: rgb(0x1a, 0x1d, 0x23),
    fg_muted: rgb(0x55, 0x60, 0x6e),
    accent: rgb(0x07, 0x59, 0x85),
    accent_fg: rgb(0xff, 0xff, 0xff),
    accent_muted: rgb(0xdc, 0xef, 0xfb),
    warn: rgb(0x9a, 0x51, 0x08),
    danger: rgb(0xdc, 0x26, 0x26),
};

/// Build web-matched visuals for the given mode, starting from egui's defaults and overriding the
/// palette so unset fields keep sane behaviour.
pub fn visuals(dark: bool) -> egui::Visuals {
    let p = if dark { &DARK } else { &LIGHT };
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    v.panel_fill = p.surface;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.extreme_bg_color = p.bg; // text-edit background / troughs
    v.faint_bg_color = p.surface2; // striped rows
    v.code_bg_color = p.surface2;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.danger;
    // Selection = subtle accent-muted fill with an accent stroke (matches the web's low-chrome feel).
    v.selection.bg_fill = p.accent_muted;
    v.selection.stroke = Stroke::new(1.0, p.accent);

    let w = &mut v.widgets;
    // Non-interactive: plain labels, separators, panel background. fg_stroke is the primary text tier.
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.fg);
    // Inactive: buttons / widgets at rest.
    w.inactive.bg_fill = p.surface2;
    w.inactive.weak_bg_fill = p.surface2;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.fg_muted);
    // Hovered.
    w.hovered.bg_fill = p.surface2;
    w.hovered.weak_bg_fill = p.surface2;
    w.hovered.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.hovered.fg_stroke = Stroke::new(1.0, p.fg);
    // Active (pressed / on) — the accent.
    w.active.bg_fill = p.accent;
    w.active.weak_bg_fill = p.accent;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.0, p.accent_fg);
    // Open (combo / menu expanded).
    w.open.bg_fill = p.surface2;
    w.open.weak_bg_fill = p.surface2;
    w.open.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.open.fg_stroke = Stroke::new(1.0, p.fg);

    v
}
