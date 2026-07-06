//! `WaveformView` — the audio waveform island.
//!
//! A "hot render path" the DOM can't do well at scale, so it is a WASM island (ADR 0009 §9), while
//! *thumbnails* stay server-rendered previews and are deliberately not here. The DOM hands mono
//! samples in `[-1, 1]` (`set_waveform`), a normalised playhead (`set_progress`), and the canvas
//! size (`resize`); the island reduces the samples to per-column min/max **peaks** on the CPU and
//! draws filled bars, colouring played vs unplayed by the playhead.
//!
//! Same wasm-bindgen contract shape as [`crate::model_viewer`]: `create` · a data-in method ·
//! `set_progress`/`resize` · `free()`. Shares the [`crate::gpu`] context and the dirty-flag rAF loop.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use crate::gpu::GpuContext;
use crate::raf::{self, RafHandle};

/// Minimum half-height (clip units) so silence still shows a centre line rather than nothing.
const MIN_HALF: f32 = 0.01;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct WaveVertex {
    pos: [f32; 2],
    x01: f32,
}

impl WaveVertex {
    const ATTRS: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32];

    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<WaveVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRS,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct WaveUniforms {
    played: [f32; 4],
    unplayed: [f32; 4],
    progress: f32,
    _pad: [f32; 3],
}

struct Inner {
    ctx: GpuContext,
    pipeline: wgpu::RenderPipeline,
    uniform_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    samples: Vec<f32>,
    vbuf: Option<wgpu::Buffer>,
    vertex_count: u32,
    progress: f32,
    dirty: bool,
}

impl Inner {
    /// Rebuild the bar geometry from the current samples at the current canvas width. Called on new
    /// data and on resize (column count follows the pixel width).
    fn rebuild(&mut self) {
        let columns = self.ctx.config.width.max(1);
        let verts = build_peaks(&self.samples, columns);
        self.vertex_count = verts.len() as u32;
        self.vbuf = if verts.is_empty() {
            None
        } else {
            Some(wgpu::util::DeviceExt::create_buffer_init(
                &self.ctx.device,
                &wgpu::util::BufferInitDescriptor {
                    label: Some("waveform-verts"),
                    contents: bytemuck::cast_slice(&verts),
                    usage: wgpu::BufferUsages::VERTEX,
                },
            ))
        };
    }

    fn render(&mut self) {
        let uniforms = WaveUniforms {
            // Accent for played, muted for the rest — tuned to DESIGN_GUIDELINES §4 dark palette.
            played: [0.36, 0.55, 0.98, 1.0],
            unplayed: [0.36, 0.40, 0.48, 1.0],
            progress: self.progress,
            _pad: [0.0; 3],
        };
        self.ctx
            .queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));

        let Some(frame) = self.ctx.acquire() else {
            return;
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("waveform-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("waveform-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.08,
                            g: 0.09,
                            b: 0.11,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(vbuf) = &self.vbuf {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, vbuf.slice(..));
                pass.draw(0..self.vertex_count, 0..1);
            }
        }
        self.ctx.queue.submit([encoder.finish()]);
        self.ctx.queue.present(frame);
    }
}

#[wasm_bindgen]
pub struct WaveformView {
    inner: Rc<RefCell<Inner>>,
    _raf: RafHandle,
}

#[wasm_bindgen]
impl WaveformView {
    /// Async GPU init against a DOM canvas; resolves to a live island (blank track until samples
    /// arrive) or rejects with a readable message.
    #[wasm_bindgen(js_name = create)]
    pub async fn create(canvas: HtmlCanvasElement) -> Result<WaveformView, JsValue> {
        let ctx = GpuContext::create(canvas)
            .await
            .map_err(|e| JsValue::from_str(&e))?;
        log::info!("dam-viewer: waveform island up on {} backend", ctx.backend);

        let device = &ctx.device;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("waveform.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/waveform.wgsl").into()),
        });
        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("waveform-uniforms"),
            size: std::mem::size_of::<WaveUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("waveform-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("waveform-bg"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("waveform-pl"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("waveform-pipeline"),
            layout: Some(&pl),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(WaveVertex::layout())],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: ctx.config.format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let inner = Rc::new(RefCell::new(Inner {
            ctx,
            pipeline,
            uniform_buf,
            bind_group,
            samples: Vec::new(),
            vbuf: None,
            vertex_count: 0,
            progress: 0.0,
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
            inner.render();
            inner.dirty = false;
        });

        Ok(WaveformView { inner, _raf: raf })
    }

    /// Hand in mono samples in `[-1, 1]` (already downmixed/decoded DOM-side). Copied and reduced to
    /// per-column peaks immediately.
    #[wasm_bindgen(js_name = setWaveform)]
    pub fn set_waveform(&self, samples: &[f32]) {
        let mut inner = self.inner.borrow_mut();
        inner.samples = samples.to_vec();
        inner.rebuild();
        inner.dirty = true;
    }

    /// Move the playhead. `progress` is clamped to `[0, 1]`.
    #[wasm_bindgen(js_name = setProgress)]
    pub fn set_progress(&self, progress: f32) {
        let mut inner = self.inner.borrow_mut();
        inner.progress = progress.clamp(0.0, 1.0);
        inner.dirty = true;
    }

    /// New canvas backing size in device pixels; rebuilds peaks at the new column count.
    pub fn resize(&self, width: u32, height: u32) {
        let mut inner = self.inner.borrow_mut();
        inner.ctx.resize(width, height);
        inner.rebuild();
        inner.dirty = true;
    }

    #[wasm_bindgen(getter)]
    pub fn backend(&self) -> String {
        self.inner.borrow().ctx.backend.to_string()
    }
}

/// Reduce samples to two filled triangles per canvas column, spanning that column's min..max
/// amplitude (with a small floor so silence is visible). Clip-space geometry; `x01` per vertex lets
/// the shader colour played vs unplayed.
fn build_peaks(samples: &[f32], columns: u32) -> Vec<WaveVertex> {
    let mut out = Vec::with_capacity(columns as usize * 6);
    let n = samples.len();
    let cols = columns as usize;
    for c in 0..cols {
        let (mut lo, mut hi) = (0.0f32, 0.0f32);
        if n > 0 {
            let start = c * n / cols;
            let end = (((c + 1) * n / cols).max(start + 1)).min(n);
            lo = 1.0;
            hi = -1.0;
            for &s in &samples[start..end] {
                lo = lo.min(s);
                hi = hi.max(s);
            }
            if lo > hi {
                lo = 0.0;
                hi = 0.0;
            }
        }
        let x0 = (c as f32 / cols as f32) * 2.0 - 1.0;
        let x1 = ((c as f32 + 1.0) / cols as f32) * 2.0 - 1.0;
        let ymin = lo.min(-MIN_HALF);
        let ymax = hi.max(MIN_HALF);
        let x01 = (c as f32 + 0.5) / cols as f32;

        let bl = WaveVertex {
            pos: [x0, ymin],
            x01,
        };
        let br = WaveVertex {
            pos: [x1, ymin],
            x01,
        };
        let tr = WaveVertex {
            pos: [x1, ymax],
            x01,
        };
        let tl = WaveVertex {
            pos: [x0, ymax],
            x01,
        };
        out.extend_from_slice(&[bl, br, tr, bl, tr, tl]);
    }
    out
}
