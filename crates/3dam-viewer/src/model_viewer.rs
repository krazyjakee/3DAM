//! `ModelViewer` — the interactive 3D viewer island.
//!
//! Implements the ratified wasm-bindgen contract (ADR 0009 §9 / tech-spec 09 §B.3):
//! `create(canvas)` (async init) · `load_model(bytes)` · `set_camera(yaw, pitch, zoom)` ·
//! `resize(w, h)` · `free()` (wasm-bindgen's generated destructor = the contract's `drop`).
//!
//! The DOM owns data and chrome: it fetches GLB bytes over the file-03 API and hands them in; orbit
//! controls are DOM and call `set_camera`. State changes just mark the view dirty — the island's own
//! rAF loop ([`crate::raf`]) redraws only when needed, so a static model costs no GPU per frame.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use crate::camera::{Aabb, OrbitCamera};
use crate::gltf_load;
use crate::gpu::GpuContext;
use crate::raf::{self, RafHandle};
use crate::scene::ModelRenderer;

struct Inner {
    ctx: GpuContext,
    renderer: ModelRenderer,
    camera: OrbitCamera,
    dirty: bool,
}

#[wasm_bindgen]
pub struct ModelViewer {
    inner: Rc<RefCell<Inner>>,
    // Dropping this (on `free()`) stops the rAF loop; `inner`'s `Rc` then hits zero and releases the
    // wgpu device/surface. Field order matters: the handle drops before `inner`.
    _raf: RafHandle,
}

#[wasm_bindgen]
impl ModelViewer {
    /// Async GPU init against a DOM canvas. Resolves to a live viewer (already running its render
    /// loop, showing the neutral background) or rejects with a readable message the wrapper can show
    /// as a "3D preview unavailable" state.
    #[wasm_bindgen(js_name = create)]
    pub async fn create(canvas: HtmlCanvasElement) -> Result<ModelViewer, JsValue> {
        let ctx = GpuContext::create(canvas).await.map_err(js_err)?;
        log::info!("dam-viewer: 3D island up on {} backend", ctx.backend);

        let renderer = ModelRenderer::new(&ctx);
        let camera = OrbitCamera::new(Aabb::empty().or_unit());
        let inner = Rc::new(RefCell::new(Inner {
            ctx,
            renderer,
            camera,
            dirty: true,
        }));

        let weak = Rc::downgrade(&inner);
        let raf = raf::start(move || {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let mut inner = inner.borrow_mut();
            if !inner.dirty {
                return;
            }
            let Inner {
                ctx,
                renderer,
                camera,
                dirty,
            } = &mut *inner;
            renderer.render(ctx, camera);
            *dirty = false;
        });

        Ok(ModelViewer { inner, _raf: raf })
    }

    /// Decode + upload glTF/GLB bytes, then re-fit the camera to the new bounds. `Err` (bad/loose
    /// glTF) leaves any previously loaded model on screen.
    #[wasm_bindgen(js_name = loadModel)]
    pub fn load_model(&self, bytes: &[u8]) -> Result<(), JsValue> {
        let cpu = gltf_load::load(bytes).map_err(js_err)?;
        log::info!(
            "dam-viewer: loaded model — {} verts, {} indices",
            cpu.vertices.len(),
            cpu.indices.len()
        );
        let mut inner = self.inner.borrow_mut();
        let Inner {
            ctx,
            renderer,
            camera,
            dirty,
        } = &mut *inner;
        renderer.upload(ctx, &cpu);
        camera.set_bounds(cpu.bounds);
        *dirty = true;
        Ok(())
    }

    /// Load a loose glTF whose buffers are external files (issue #56): `json` is the `.gltf` text and
    /// `buffers` is a JS array of `Uint8Array`, one per glTF buffer in index order, already resolved
    /// by the DOM (data-URIs decoded, external files fetched via `/assets/{id}/related`). Same upload +
    /// re-fit as `loadModel`.
    #[wasm_bindgen(js_name = loadGltfExternal)]
    pub fn load_gltf_external(&self, json: &[u8], buffers: js_sys::Array) -> Result<(), JsValue> {
        use wasm_bindgen::JsCast;
        let mut bufs: Vec<Vec<u8>> = Vec::with_capacity(buffers.length() as usize);
        for v in buffers.iter() {
            let arr: js_sys::Uint8Array = v
                .dyn_into()
                .map_err(|_| js_err("buffers must be Uint8Array".to_string()))?;
            bufs.push(arr.to_vec());
        }
        let cpu = gltf_load::load_external(json, bufs).map_err(js_err)?;
        log::info!(
            "dam-viewer: loaded loose glTF — {} verts, {} indices",
            cpu.vertices.len(),
            cpu.indices.len()
        );
        let mut inner = self.inner.borrow_mut();
        let Inner {
            ctx,
            renderer,
            camera,
            dirty,
        } = &mut *inner;
        renderer.upload(ctx, &cpu);
        camera.set_bounds(cpu.bounds);
        *dirty = true;
        Ok(())
    }

    /// DOM-driven orbit + zoom. `yaw`/`pitch` in radians; `zoom` multiplies the bounds-fit distance.
    #[wasm_bindgen(js_name = setCamera)]
    pub fn set_camera(&self, yaw: f32, pitch: f32, zoom: f32) {
        let mut inner = self.inner.borrow_mut();
        inner.camera.update(yaw, pitch, zoom);
        inner.dirty = true;
    }

    /// New canvas backing size in device pixels (CSS owns layout; call on container resize).
    pub fn resize(&self, width: u32, height: u32) {
        let mut inner = self.inner.borrow_mut();
        inner.ctx.resize(width, height);
        inner.dirty = true;
    }

    /// The backend actually chosen — `"webgpu"` or `"webgl2"`. For the web client to log/telemeter
    /// which path a browser took.
    #[wasm_bindgen(getter)]
    pub fn backend(&self) -> String {
        self.inner.borrow().ctx.backend.to_string()
    }

    /// Whether a model has been successfully loaded (vs just the empty background).
    #[wasm_bindgen(js_name = hasModel)]
    pub fn has_model(&self) -> bool {
        self.inner.borrow().renderer.has_model()
    }
}

fn js_err(e: String) -> JsValue {
    JsValue::from_str(&e)
}
