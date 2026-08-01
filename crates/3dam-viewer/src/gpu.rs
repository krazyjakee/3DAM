//! Shared `wgpu` context for both islands: instance → surface → adapter → device/queue → config.
//!
//! **WebGPU with WebGL2 fallback** (ADR 0009 §9). The instance enables both `BROWSER_WEBGPU` and
//! `GL`; wgpu picks WebGPU when the browser exposes it and transparently falls back to WebGL2
//! otherwise. We request the **WebGL2 downlevel limits** so a pipeline built here is valid on
//! *either* backend — WebGPU is a strict superset, so the constraint costs nothing there but keeps
//! us honest about what the fallback can do.
//!
//! No windowing, no event loop — the canvas is handed in by the DOM (tech-spec 09 §B.3). This
//! mirrors the surface-less headless path validated in `spikes/headless-render`, differing only in
//! that the browser target *has* a surface (the canvas) to present to.

use web_sys::HtmlCanvasElement;

use crate::render_quality::common_sample_count;

/// Web-compatible depth attachment. The viewer does not sample depth; 24-bit depth gives WebGL2 a
/// broader multisample path than `Depth32Float` while retaining ample precision for fitted bounds.
pub const VIEWER_DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth24Plus;

/// Everything the render paths need to draw one frame into a canvas.
pub struct GpuContext {
    /// `'static` because [`wgpu::SurfaceTarget::Canvas`] clones the canvas handle into the surface.
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
    /// Adapter-supported MSAA count for the configured surface format. Both WebGPU and WebGL2 use
    /// 4× when available, otherwise the renderer honestly reports single-sample fallback.
    pub sample_count: u32,
    /// Human-readable backend actually chosen (`"webgpu"` / `"webgl2"` / …), surfaced to JS so the
    /// web client can log which path a browser landed on.
    pub backend: &'static str,
}

impl GpuContext {
    /// One-time async GPU init against a DOM canvas. `Err(String)` on any unrecoverable step so the
    /// island can reject its `create()` promise with a readable message (the React wrapper then
    /// shows a "3D preview unavailable" fallback rather than a blank canvas).
    pub async fn create(canvas: HtmlCanvasElement) -> Result<Self, String> {
        let width = canvas.width().max(1);
        let height = canvas.height().max(1);

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::BROWSER_WEBGPU | wgpu::Backends::GL,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        let surface = instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas))
            .map_err(|e| format!("create_surface failed: {e}"))?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
                apply_limit_buckets: false,
            })
            .await
            .map_err(|e| format!("no GPU adapter (WebGPU + WebGL2 both unavailable): {e}"))?;

        let backend = backend_name(adapter.get_info().backend);

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("dam-viewer-device"),
                // Valid on WebGL2 *and* WebGPU — see module docs.
                required_limits: wgpu::Limits::downlevel_webgl2_defaults(),
                ..Default::default()
            })
            .await
            .map_err(|e| format!("request_device failed: {e}"))?;

        let mut config = surface
            .get_default_config(&adapter, width, height)
            .ok_or_else(|| "surface is not supported by the chosen adapter".to_string())?;
        config.usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        let color = adapter.get_texture_format_features(config.format).flags;
        let depth = adapter
            .get_texture_format_features(VIEWER_DEPTH_FORMAT)
            .flags;
        let sample_count = common_sample_count(
            color.sample_count_supported(4),
            color.sample_count_supported(2),
            depth.sample_count_supported(4),
            depth.sample_count_supported(2),
        );
        surface.configure(&device, &config);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            sample_count,
            backend,
        })
    }

    /// Re-configure the swapchain for a new canvas backing size (device pixels). The DOM/CSS owns
    /// layout and calls this on container resize (tech-spec 09 §B.3). A zero dimension (hidden
    /// element) is ignored so we never configure a 0×0 surface, which wgpu panics on.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if width == self.config.width && height == self.config.height {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Acquire the next swapchain texture, transparently recovering from a stale/outdated surface
    /// (common on WebGL after a resize) by reconfiguring once. Returns `None` if the frame should
    /// simply be skipped (timeout/occluded/lost) — the caller reschedules.
    pub fn acquire(&mut self) -> Option<wgpu::SurfaceTexture> {
        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => Some(t),
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => Some(t),
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(t)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(t) => Some(t),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Current aspect ratio (width / height), guarded against a zero height.
    pub fn aspect(&self) -> f32 {
        self.config.width as f32 / self.config.height.max(1) as f32
    }
}

fn backend_name(b: wgpu::Backend) -> &'static str {
    match b {
        wgpu::Backend::BrowserWebGpu => "webgpu",
        wgpu::Backend::Gl => "webgl2",
        wgpu::Backend::Vulkan => "vulkan",
        wgpu::Backend::Metal => "metal",
        wgpu::Backend::Dx12 => "dx12",
        wgpu::Backend::Noop => "noop",
    }
}
