//! Web-matched egui theme (issue #68).
//!
//! Builds concrete [`egui::Visuals`] from the web client's Tailwind `@theme` design tokens
//! (`web/src/index.css`, governed by `docs/DESIGN_GUIDELINES.md`) so the native GUI and the web app
//! read as the same product: dark-first, low-chrome, information-dense, one sky accent (`#38bdf8`),
//! with warn/danger reserved for exposure risk. The rail Theme control picks a preference
//! (System/Dark/Light); this module decides the concrete colours each resolves to.
//!
//! Beyond the [`egui::Visuals`] palette this module also owns:
//! * the extended design tokens egui has no slot for — per-media identity colours and license-badge
//!   hues — surfaced through [`colors`] for the badge/chip helpers in [`crate::ui`];
//! * [`install_fonts`] / [`install_style`], which register Inter + Phosphor and the web's type scale
//!   and spacing so the shell stops rendering in egui's stock font at default metrics.

use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke,
    TextStyle,
};

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// Inter Variable — the exact face the web client loads (`@fontsource-variable/inter`). Bundled so
/// the native shell is not at the mercy of a system font; rendered at its default (Regular) instance.
const INTER: &[u8] = include_bytes!("../assets/fonts/InterVariable.ttf");

/// Mode-independent surface behind media previews (3D viewer, waveform trough). Like the web's WASM
/// islands — which clear to a fixed dark neutral in both themes — preview surfaces stay dark so the
/// rendered content reads the same regardless of the shell theme.
pub const VIEWER_BG: Color32 = rgb(0x0f, 0x17, 0x21);

/// One mode's design tokens (names mirror the web `--color-*` custom properties). This is the full
/// token set — the subset egui's `Visuals` can hold is copied into it in [`visuals`]; the rest
/// (media + license hues) is read directly through [`colors`].
#[derive(Clone, Copy)]
pub struct Colors {
    pub bg: Color32,
    pub surface: Color32,
    pub surface2: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub fg: Color32,
    pub fg_muted: Color32,
    pub fg_dim: Color32,
    pub accent: Color32,
    pub accent_fg: Color32,
    pub accent_muted: Color32,
    pub warn: Color32,
    pub danger: Color32,
    pub ok: Color32,
    // Per-media identity hues (badges + typed tiles) — not the accent.
    pub media_audio: Color32,
    pub media_image: Color32,
    pub media_model: Color32,
    // License-badge hues (colour + dot), matching the web LicenseBadge.
    pub lic_permissive: Color32,
    pub lic_attribution: Color32,
    pub lic_restricted: Color32,
    pub lic_unknown: Color32,
}

// Dark palette — the `@theme` block in web/src/index.css (dark-first, DESIGN_GUIDELINES §4).
const DARK: Colors = Colors {
    bg: rgb(0x0b, 0x0d, 0x10),
    surface: rgb(0x14, 0x17, 0x1c),
    surface2: rgb(0x1b, 0x1f, 0x26),
    border: rgb(0x26, 0x2b, 0x33),
    border_strong: rgb(0x33, 0x3a, 0x45),
    fg: rgb(0xe6, 0xe9, 0xee),
    fg_muted: rgb(0x9a, 0xa3, 0xb0),
    fg_dim: rgb(0x86, 0x8f, 0x9c),
    accent: rgb(0x38, 0xbd, 0xf8),
    accent_fg: rgb(0x05, 0x10, 0x16),
    accent_muted: rgb(0x0e, 0x2b, 0x3a),
    warn: rgb(0xf5, 0xa5, 0x24),
    danger: rgb(0xf0, 0x61, 0x6d),
    ok: rgb(0x4a, 0xde, 0x80),
    media_audio: rgb(0x35, 0xd0, 0xba),
    media_image: rgb(0xf0, 0x88, 0x3e),
    media_model: rgb(0x7c, 0x8c, 0xff),
    lic_permissive: rgb(0x4a, 0xde, 0x80),
    lic_attribution: rgb(0x38, 0xbd, 0xf8),
    lic_restricted: rgb(0xf0, 0x61, 0x6d),
    lic_unknown: rgb(0x8b, 0x93, 0xa1),
};

// Light palette — the `.light`/`:root[data-theme=light]` overrides in web/src/index.css.
const LIGHT: Colors = Colors {
    bg: rgb(0xf6, 0xf7, 0xf9),
    surface: rgb(0xff, 0xff, 0xff),
    surface2: rgb(0xec, 0xee, 0xf1),
    border: rgb(0xda, 0xdf, 0xe6),
    border_strong: rgb(0xc2, 0xc9, 0xd3),
    fg: rgb(0x1a, 0x1d, 0x23),
    fg_muted: rgb(0x55, 0x60, 0x6e),
    fg_dim: rgb(0x5d, 0x66, 0x73),
    accent: rgb(0x07, 0x59, 0x85),
    accent_fg: rgb(0xff, 0xff, 0xff),
    accent_muted: rgb(0xdc, 0xef, 0xfb),
    warn: rgb(0x9a, 0x51, 0x08),
    danger: rgb(0xdc, 0x26, 0x26),
    ok: rgb(0x15, 0x80, 0x3d),
    media_audio: rgb(0x0f, 0x76, 0x6e),
    media_image: rgb(0xc2, 0x41, 0x0c),
    media_model: rgb(0x4f, 0x46, 0xe5),
    lic_permissive: rgb(0x15, 0x80, 0x3d),
    lic_attribution: rgb(0x07, 0x59, 0x85),
    lic_restricted: rgb(0xdc, 0x26, 0x26),
    lic_unknown: rgb(0x6b, 0x72, 0x80),
};

/// The full extended token set for the given mode (for the badge/chip helpers that egui's `Visuals`
/// can't carry). Cheap to call per-frame — it just returns a `Copy` of the const palette.
pub fn colors(dark: bool) -> Colors {
    if dark {
        DARK
    } else {
        LIGHT
    }
}

/// Build web-matched visuals for the given mode, starting from egui's defaults and overriding the
/// palette so unset fields keep sane behaviour.
pub fn visuals(dark: bool) -> egui::Visuals {
    let p = colors(dark);
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
    // Rounding: web uses 4px (`rounded`) on controls/cards, 6px (`rounded-md`) on floating surfaces.
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(6);
    // Low-chrome: no expansion halo on hover, a soft popup shadow only.
    v.popup_shadow = egui::epaint::Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(80),
    };
    v.window_shadow = v.popup_shadow;
    // Selection = subtle accent-muted fill with an accent stroke (matches the web's low-chrome feel).
    v.selection.bg_fill = p.accent_muted;
    v.selection.stroke = Stroke::new(1.0, p.accent);

    let round = CornerRadius::same(4);
    let w = &mut v.widgets;
    // Non-interactive: plain labels, separators, panel background. fg_stroke is the primary text tier.
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.fg);
    w.noninteractive.corner_radius = round;
    // Inactive: buttons / widgets at rest — surface2 fill, muted label (the web `.btn`).
    w.inactive.bg_fill = p.surface2;
    w.inactive.weak_bg_fill = p.surface2;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.fg_muted);
    w.inactive.corner_radius = round;
    w.inactive.expansion = 0.0;
    // Hovered — text brightens, border strengthens (web `.btn:hover`), fill unchanged.
    w.hovered.bg_fill = p.surface2;
    w.hovered.weak_bg_fill = p.surface2;
    w.hovered.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.hovered.fg_stroke = Stroke::new(1.0, p.fg);
    w.hovered.corner_radius = round;
    w.hovered.expansion = 0.0;
    // Active (pressed / on) — the accent.
    w.active.bg_fill = p.accent;
    w.active.weak_bg_fill = p.accent;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.0, p.accent_fg);
    w.active.corner_radius = round;
    w.active.expansion = 0.0;
    // Open (combo / menu expanded).
    w.open.bg_fill = p.surface2;
    w.open.weak_bg_fill = p.surface2;
    w.open.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.open.fg_stroke = Stroke::new(1.0, p.fg);
    w.open.corner_radius = round;

    v
}

/// Register Inter (the web's face) as the primary proportional font and Phosphor as the icon face,
/// keeping egui's bundled fonts as fallbacks for any glyph Inter lacks. Called once at startup.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts
        .font_data
        .insert("Inter".to_owned(), Arc::new(FontData::from_static(INTER)));
    // Phosphor appends its own font-data + family entries (icon glyphs live in a private range).
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    // Inter leads the proportional family; the default fonts stay behind it as fallbacks.
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "Inter".to_owned());
    ctx.set_fonts(fonts);
}

/// Apply the web type scale (13px base) and spacing/density so widgets stop rendering at egui's
/// default metrics. Colours live in [`visuals`]; this is font sizes + spacing only, set once.
pub fn install_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    // Web: 13px body, 10–11px fine labels, 14–15px headings. Proportional = Inter after install_fonts.
    style.text_styles = [
        (
            TextStyle::Small,
            FontId::new(11.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(13.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Heading,
            FontId::new(15.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(12.0, FontFamily::Monospace),
        ),
    ]
    .into();
    let s = &mut style.spacing;
    s.item_spacing = egui::vec2(8.0, 6.0);
    s.button_padding = egui::vec2(9.0, 4.0);
    s.menu_margin = Margin::same(6);
    s.interact_size.y = 24.0;
    s.icon_width = 16.0;
    s.icon_width_inner = 10.0;
    s.window_margin = Margin::same(10);
    // Slimmer scrollbars in the web's gutter style.
    s.scroll.bar_width = 8.0;
    s.scroll.bar_inner_margin = 2.0;
    ctx.set_style(style);
}
