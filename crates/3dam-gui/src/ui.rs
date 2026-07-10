//! Web-matched UI primitives: the small painted widgets the shell repeats everywhere — section
//! labels, media/license badges, count chips and full-width navigation rows — factored out so the
//! native GUI reads like the React client (`web/src/components`) rather than a wall of stock
//! `selectable_label`s. Colours come from [`crate::theme::colors`]; nothing here holds state.

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontFamily, FontId, Rect, Response, Sense, Stroke,
    TextStyle, Vec2,
};

use crate::theme::{colors, Colors};

/// Phosphor icon glyphs (the lucide-equivalent set registered in [`crate::theme::install_fonts`]).
/// Import as `use crate::ui::icon;` then reference e.g. `icon::MAGNIFYING_GLASS`.
pub use egui_phosphor::regular as icon;

/// Blend `fg` over a transparent base at `pct` (0..=1) — the web's `color-mix(currentColor N%,
/// transparent)` used for tinted badge backgrounds.
pub fn tint(fg: Color32, pct: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(fg.r(), fg.g(), fg.b(), (pct * 255.0) as u8)
}

/// An uppercase, letter-spaced, dim section label (web `text-[10px] tracking-wider uppercase
/// text-fg-dim`). Adds a little breathing room above, matching the sidebar's `pt-4`.
pub fn section_label(ui: &mut egui::Ui, text: &str) {
    let c = colors(ui.visuals().dark_mode);
    ui.add_space(6.0);
    let spaced: String = text
        .to_uppercase()
        .chars()
        .flat_map(|ch| [ch, '\u{200a}'])
        .collect();
    ui.label(
        egui::RichText::new(spaced)
            .color(c.fg_dim)
            .size(10.0)
            .family(FontFamily::Proportional),
    );
    ui.add_space(1.0);
}

/// A filled rounded pill (badge). Returns the response so callers can add tooltips/clicks.
pub fn pill(ui: &mut egui::Ui, text: &str, fg: Color32, bg: Color32) -> Response {
    let font = FontId::new(10.0, FontFamily::Proportional);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, fg);
    let pad = Vec2::new(5.0, 2.0);
    let size = galley.size() + pad * 2.0;
    let (rect, resp) = ui.allocate_at_least(size, Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(3), bg);
    p.galley(rect.min + pad, galley, fg);
    resp
}

/// The identity hue for a media kind (`audio`/`image`/`model`, tolerant of variants).
pub fn media_color(c: &Colors, media: &str) -> Color32 {
    match media.chars().next().map(|ch| ch.to_ascii_lowercase()) {
        Some('a') => c.media_audio,
        Some('i') => c.media_image,
        Some('m') => c.media_model,
        _ => c.fg_dim,
    }
}

/// Short uppercase tag for a media kind (matches the web MediaBadge text: SFX / IMG / 3D).
pub fn media_tag(media: &str) -> &'static str {
    match media.chars().next().map(|ch| ch.to_ascii_lowercase()) {
        Some('a') => "SFX",
        Some('i') => "IMG",
        Some('m') => "3D",
        _ => "?",
    }
}

/// A colour-coded media badge (tinted background + coloured text), like the grid/table MediaBadge.
pub fn media_badge(ui: &mut egui::Ui, media: &str) -> Response {
    let c = colors(ui.visuals().dark_mode);
    let col = media_color(&c, media);
    pill(ui, media_tag(media), col, tint(col, 0.16))
}

/// The hue for a license class (permissive/attribution/restricted/unknown, tolerant of full names).
pub fn license_color(c: &Colors, license: &str) -> Color32 {
    let l = license.to_ascii_lowercase();
    if l.contains("permiss") {
        c.lic_permissive
    } else if l.contains("attrib") {
        c.lic_attribution
    } else if l.contains("restrict") || l.contains("gpl") || l.contains("noncommer") {
        c.lic_restricted
    } else {
        c.lic_unknown
    }
}

/// A license badge: a coloured dot followed by the label, in the license hue (web LicenseBadge).
pub fn license_badge(ui: &mut egui::Ui, license: &str) -> Response {
    let c = colors(ui.visuals().dark_mode);
    let col = license_color(&c, license);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let (dot, _) = ui.allocate_exact_size(Vec2::splat(7.0), Sense::hover());
        ui.painter().circle_filled(dot.center(), 3.0, col);
        ui.label(egui::RichText::new(license).color(col).size(11.0));
    })
    .response
}

/// Compact human count (1234 → "1.2k") for dense chips.
pub fn fmt_count(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 10_000 {
        format!("{}k", n / 1000)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// A full-width navigation row: `[icon] label ........ count`, with the web's active
/// (accent-muted fill + accent text) and hover (surface2 fill) states. `icon_color`, when set,
/// tints the leading glyph (used for the media/license identity dots); otherwise it follows the text.
pub fn nav_row(
    ui: &mut egui::Ui,
    glyph: &str,
    label: &str,
    count: Option<i64>,
    active: bool,
    icon_color: Option<Color32>,
) -> Response {
    let c = colors(ui.visuals().dark_mode);
    let full_w = ui.available_width();
    let (rect, resp) = ui.allocate_at_least(Vec2::new(full_w, 24.0), Sense::click());
    let hovered = resp.hovered();

    let p = ui.painter();
    if active {
        p.rect_filled(rect, CornerRadius::same(4), c.accent_muted);
    } else if hovered {
        p.rect_filled(rect, CornerRadius::same(4), c.surface2);
    }
    let fg = if active {
        c.accent
    } else if hovered {
        c.fg
    } else {
        c.fg_muted
    };

    let mut x = rect.left() + 9.0;
    if !glyph.is_empty() {
        let icol = icon_color.unwrap_or(fg);
        let gid = FontId::new(15.0, FontFamily::Proportional);
        let g = p.layout_no_wrap(glyph.to_owned(), gid, icol);
        let gy = rect.center().y - g.size().y / 2.0;
        p.galley(egui::pos2(x, gy), g, icol);
        x += 15.0 + 8.0;
    }

    // Right-aligned count first so the label knows how much room it has.
    let mut right = rect.right() - 8.0;
    if let Some(n) = count {
        let cf = FontId::new(11.0, FontFamily::Proportional);
        let cg = p.layout_no_wrap(fmt_count(n), cf, c.fg_dim);
        let cx = right - cg.size().x;
        let cy = rect.center().y - cg.size().y / 2.0;
        p.galley(egui::pos2(cx, cy), cg, c.fg_dim);
        right = cx - 6.0;
    }

    let lf = TextStyle::Body.resolve(ui.style());
    let avail = (right - x).max(0.0);
    let lg = p.layout(label.to_owned(), lf, fg, avail);
    let ly = rect.center().y - lg.size().y / 2.0;
    p.galley(egui::pos2(x, ly), lg, fg);

    resp
}

/// A left-anchored, full-width label row used for section footers (Duplicates / Blocklist /
/// Settings) — icon + label with hover feedback, no trailing count.
pub fn link_row(ui: &mut egui::Ui, glyph: &str, label: &str, active: bool) -> Response {
    nav_row(ui, glyph, label, None, active, None)
}

/// Paint a 1px hairline separator in the border colour across the available width.
pub fn hairline(ui: &mut egui::Ui) {
    let c = colors(ui.visuals().dark_mode);
    ui.add_space(4.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.center().y, Stroke::new(1.0, c.border));
    ui.add_space(4.0);
}

/// A key/value metadata row for the inspector: dim label left, value right (wrapping/truncating).
pub fn meta_row(ui: &mut egui::Ui, label: &str, value: &str) {
    let c = colors(ui.visuals().dark_mode);
    ui.horizontal(|ui| {
        ui.add(
            egui::Label::new(egui::RichText::new(label).color(c.fg_dim).size(12.0))
                .selectable(false),
        );
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.add(egui::Label::new(egui::RichText::new(value).color(c.fg).size(12.0)).truncate());
        });
    });
}

/// A 0..=1 feature bar (Seamlessness / Brightness / Harmonicity) rendered as a filled accent track,
/// matching the web inspector's thin progress bars, with the value labelled on the right.
pub fn feature_bar(ui: &mut egui::Ui, label: &str, v: f32) {
    let c = colors(ui.visuals().dark_mode);
    ui.horizontal(|ui| {
        ui.add(
            egui::Label::new(egui::RichText::new(label).color(c.fg_dim).size(11.0))
                .selectable(false),
        );
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                egui::RichText::new(format!("{:.0}%", v * 100.0))
                    .color(c.fg_muted)
                    .size(11.0),
            );
            let w = ui.available_width().max(24.0);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 6.0), Sense::hover());
            let p = ui.painter();
            p.rect_filled(rect, CornerRadius::same(3), c.surface2);
            let fill = Rect::from_min_size(
                rect.min,
                Vec2::new(rect.width() * v.clamp(0.0, 1.0), rect.height()),
            );
            p.rect_filled(fill, CornerRadius::same(3), c.accent);
        });
    });
}
