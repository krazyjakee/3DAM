# Web ↔ native parity — closed out (ADR 0013)

**This checklist is retired.** As of 2026-07-16 the native desktop app is a Tauri webview shell
(`crates/3dam-desktop`, [ADR 0013](adr/0013-desktop-shell-tauri.md)) over the same embedded React
client that `3dam serve` ships — in embedded mode it boots that server in-process (loopback,
ephemeral port) and renders it in a native window. There is **one UI codebase** (`web/`), so web ↔
native parity is structural: a user-facing feature landed in the web client is, by construction, in
the desktop app.

What replaced the per-feature tracking that used to live here:

- **Build the feature once, in `web/`** — it ships to the browser, the hosted thin client, and the
  desktop shell simultaneously.
- **Both form factors still matter.** The desktop webview is a `lg+` desktop viewport; the
  responsive/touch collapse (Drawers, `coarse:` tap targets) covers mobile browsers. Keep both
  working (DESIGN_GUIDELINES).
- **Desktop-only affordances** (native folder pickers, tray, OS integration) are additive Tauri
  work in `crates/3dam-desktop`, not a parallel UI.

The egui client this file used to track (ADR 0005) reached substantive parity before retirement —
its code (`crates/3dam-gui`) and this file's full checklist are in git history at the
pre-ADR-0013 commits if ever needed.
