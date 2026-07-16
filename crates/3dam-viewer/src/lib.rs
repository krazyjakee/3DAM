//! `dam-viewer` — the browser WASM/`wgpu` viewer **islands** for the web client.
//!
//! **WASM viewer islands:** the parts the DOM/CSS chrome can't do well are
//! shipped as focused `wgpu` components embedded *in* the React layout — **not** a full-page
//! canvas ([tech-spec 09] §B.3, [ADR 0009] §9). Two islands live here:
//!
//! - [`ModelViewer`] — the interactive 3D viewer (server-decoded `DMSH` mesh → textured PBR orbit
//!   view; every Assimp format, with materials).
//! - [`WaveformView`] — the audio waveform (a "hot render path", also a WASM island per ADR 0009 §9;
//!   thumbnails stay *server-rendered* previews and are deliberately **not** here).
//!
//! **The wasm-bindgen contract** ratified in ADR 0009 §9 / tech-spec 09 §B.3 is:
//! `init(canvas) · load_preview_mesh(bytes) / set_waveform(samples) · set_camera(..) · resize(w,h) · drop()`.
//! Each island maps that onto an exported struct: `create()` (async init), a data-in method,
//! `set_camera`/`set_progress`, `resize`, and `free()` (wasm-bindgen's generated destructor = `drop`).
//!
//! **The DOM owns the data and the chrome; the island owns pixels** (tech-spec 09 §B.3). The React
//! wrapper fetches model bytes / waveform samples over the file-03 API and hands them across this
//! boundary; camera/transport controls are DOM and call in. The island does **no** networking.
//!
//! **Relationship to `dam-render` (ADR 0002).** The GPU internals here (PBR-lite shader, one draw
//! loop, versioned bounds auto-fit framing) are the *browser embodiment* of the [tech-spec 06]
//! renderer. The shared pieces (camera framing math, the `pbr.wgsl` set) reconcile with it so the
//! web viewer and the headless thumbnailer frame and shade "the same way by construction". Kept
//! self-contained (separate wgpu instances, wasm vs native). Since ADR 0013 these islands are also
//! the desktop app's viewers — the Tauri shell renders the same web client.
//!
//! [tech-spec 09]: ../../../docs/tech-spec/09-server-and-web-client.md
//! [tech-spec 06]: ../../../docs/tech-spec/06-3d-render.md
//! [ADR 0009]: ../../../docs/adr/0009-v1-scope-decisions.md
//! [ADR 0002]: ../../../docs/adr/0002-3d-render-crate-boundary.md

// The islands only exist on the browser target. On a native host this crate compiles to a tiny
// stub so `cargo build --workspace` / clippy / test stay green without pulling wgpu + web-sys into
// the server/CLI/GUI build (tech-spec 01 crate boundaries). The real code is in the `wasm` modules.
#[cfg(not(target_arch = "wasm32"))]
mod native_stub {
    /// The islands are a `wasm32-unknown-unknown` artifact built by `cargo xtask wasm`
    /// (wasm-pack). There is nothing to run on a native target.
    pub const NOT_ON_NATIVE: &str =
        "dam-viewer is a wasm32 browser artifact; build it with `cargo xtask wasm`";
}

#[cfg(not(target_arch = "wasm32"))]
pub use native_stub::NOT_ON_NATIVE;

#[cfg(target_arch = "wasm32")]
mod camera;
#[cfg(target_arch = "wasm32")]
mod gpu;
#[cfg(target_arch = "wasm32")]
mod model_viewer;
#[cfg(target_arch = "wasm32")]
mod preview_mesh;
#[cfg(target_arch = "wasm32")]
mod raf;
#[cfg(target_arch = "wasm32")]
mod scene;
#[cfg(target_arch = "wasm32")]
mod waveform;

/// Magic prefix of the server's `DMSH` preview blob (see `dam-render`'s `preview` module). Shared by
/// the parser; kept crate-level so it stays in lockstep with the serializer's constant.
#[cfg(target_arch = "wasm32")]
pub(crate) const DMSH_MAGIC: &[u8; 4] = b"DMSH";

#[cfg(target_arch = "wasm32")]
pub use model_viewer::ModelViewer;
#[cfg(target_arch = "wasm32")]
pub use waveform::WaveformView;

/// Module-load hook (wasm-bindgen runs this once when the ES module is imported). Installs the
/// panic hook so a Rust panic surfaces as a readable JS console error instead of an opaque
/// `unreachable`, and wires `log` → `console`. Idempotent and cheap; safe to run before any island
/// is created.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn __start() {
    console_error_panic_hook::set_once();
    // `Info` by default; the web app can bump it via the query string later if needed. Errors from
    // a double-init are ignored — the island module may be imported more than once across HMR.
    let _ = console_log::init_with_level(log::Level::Info);
}
