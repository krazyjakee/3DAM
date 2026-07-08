//! GPU scene + the textured metallic-roughness draw loop for the 3D island.
//!
//! The *same shape* as `dam-render`'s headless renderer (tech-spec 06 §9): one WGSL set (`pbr.wgsl`,
//! ported from `crates/3dam-render/src/shaders/pbr.wgsl`), per-material textured draws, a fixed
//! studio light rig. The point is consistent framing/shading with the server thumbnail — the browser
//! now decodes the *same* `DMSH` blob the thumbnail came from, so the interactive view and the grid
//! tile agree by construction. Uses 4× MSAA under WebGPU (resolving an off-screen colour target to
//! the swapchain) and falls back to single-sample on WebGL2, which can't multisample-resolve an
//! off-screen target; the headless path keeps its own MSAA + mip chains for higher-fidelity stills.

use wgpu::util::DeviceExt;

use crate::camera::OrbitCamera;
use crate::gpu::GpuContext;
use crate::preview_mesh::{CpuMaterial, CpuModel, CpuTexture};

/// Interleaved vertex — identical layout to `dam-render`'s `Vertex`, so the `DMSH` blob's vertex
/// runs upload verbatim. `tangent.w` carries the bitangent handedness for normal mapping.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    pub tangent: [f32; 4],
    pub uv: [f32; 2],
    pub color: [f32; 4],
}

impl Vertex {
    const ATTRS: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![
        0 => Float32x3, 1 => Float32x3, 2 => Float32x4, 3 => Float32x2, 4 => Float32x4
    ];

    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRS,
        }
    }
}

/// Camera + output-encoding uniform block. `std140`-friendly: `mat4`, then two `vec4`s (eye pos;
/// `params.x` = 1 when the surface is already sRGB so the shader writes linear and lets the hardware
/// encode, else the shader gamma-encodes itself). 96 bytes, 16-aligned.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
    camera_pos: [f32; 4],
    params: [f32; 4],
}

/// Per-material factor block, mirroring `dam-render`'s `MaterialU`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct MaterialU {
    base_color: [f32; 4],
    mr: [f32; 4], // x = metallic, y = roughness, z = alpha cutoff, w = alpha mode (0/1/2)
    emissive: [f32; 4], // rgb factor
    flags: [f32; 4], // x has_base, y has_mr, z has_normal, w has_emissive
}

const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const SRGB: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const LINEAR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Uploaded geometry for one submesh: two buffers, an index count, its material slot, whether that
/// material is blended (glass), and a centroid for back-to-front sorting of the blended pass.
struct GpuMesh {
    vbuf: wgpu::Buffer,
    ibuf: wgpu::Buffer,
    index_count: u32,
    material: usize,
    blend: bool,
    centroid: glam::Vec3,
}

/// 1×1 fallback textures for absent material maps (same neutral values as the headless renderer).
struct Defaults {
    white_srgb: wgpu::TextureView,
    white_linear: wgpu::TextureView,
    normal: wgpu::TextureView,
    black_srgb: wgpu::TextureView,
}

/// Owns the pipeline, the per-frame globals, the depth target, and (once a model is loaded) the
/// per-submesh GPU meshes + per-material bind groups. `render()` is the single draw loop.
pub struct ModelRenderer {
    /// Opaque + mask surfaces: replace blend, depth-write on.
    pipeline: wgpu::RenderPipeline,
    /// Blended (glass) surfaces: alpha blend, depth-test on / depth-write off, drawn back-to-front.
    pipeline_blend: wgpu::RenderPipeline,
    globals_buf: wgpu::Buffer,
    globals_group: wgpu::BindGroup,
    material_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    defaults: Defaults,
    depth_view: wgpu::TextureView,
    /// The MSAA colour target (`Some` only under WebGPU, where `sample_count > 1`); the pass renders
    /// into it and resolves to the swapchain. `None` on the WebGL2 fallback (single-sample direct).
    msaa_view: Option<wgpu::TextureView>,
    target_size: (u32, u32),
    sample_count: u32,
    color_format: wgpu::TextureFormat,
    meshes: Vec<GpuMesh>,
    materials: Vec<wgpu::BindGroup>,
    fallback_material: wgpu::BindGroup,
    srgb_output: f32,
}

impl ModelRenderer {
    pub fn new(ctx: &GpuContext) -> Self {
        let device = &ctx.device;

        // MSAA only under WebGPU: the WebGL2 fallback can't multisample an off-screen colour target
        // and resolve it, so it stays single-sample (the raised preview-texture cap still sharpens
        // it). 4× is universally supported on WebGPU for the swapchain formats we use.
        let sample_count = if ctx.backend == "webgpu" { 4 } else { 1 };

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pbr.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/pbr.wgsl").into()),
        });

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewer-globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let globals_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viewer-globals-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let globals_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("viewer-globals"),
            layout: &globals_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buf.as_entire_binding(),
            }],
        });

        // Material bind group: uniform factors (0), one sampler (1), four textures (2–5) — matching
        // `pbr.wgsl`'s `@group(1)` bindings.
        let tex_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let material_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viewer-material-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                tex_entry(2),
                tex_entry(3),
                tex_entry(4),
                tex_entry(5),
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("viewer-pl"),
            bind_group_layouts: &[Some(&globals_bgl), Some(&material_bgl)],
            immediate_size: 0,
        });

        // Opaque and blend variants share everything but the blend state + depth-write. Blended glass
        // depth-tests against the opaque pass (so it's occluded correctly) but never writes depth, and
        // is drawn back-to-front at render time so overlapping panes composite in the right order.
        let make_pipeline = |blend: wgpu::BlendState, depth_write: bool| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("viewer-pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[Some(Vertex::layout())],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: ctx.config.format,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    // Two-sided: game assets often ship inconsistent winding (matches dam-render).
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(depth_write),
                    depth_compare: Some(wgpu::CompareFunction::Less),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState {
                    count: sample_count,
                    ..Default::default()
                },
                multiview_mask: None,
                cache: None,
            })
        };
        let pipeline = make_pipeline(wgpu::BlendState::REPLACE, true);
        let pipeline_blend = make_pipeline(wgpu::BlendState::ALPHA_BLENDING, false);

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("viewer-sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let defaults = Defaults {
            white_srgb: pixel(ctx, [255, 255, 255, 255], SRGB),
            white_linear: pixel(ctx, [255, 255, 255, 255], LINEAR),
            normal: pixel(ctx, [128, 128, 255, 255], LINEAR),
            black_srgb: pixel(ctx, [0, 0, 0, 255], SRGB),
        };

        // Default material for submeshes whose material index is out of range.
        let fallback_material = build_material(
            device,
            &ctx.queue,
            &material_bgl,
            &sampler,
            &defaults,
            &CpuMaterial {
                base_color: [0.8, 0.8, 0.82, 1.0],
                metallic: 0.0,
                roughness: 0.6,
                emissive: [0.0; 3],
                base: None,
                mr: None,
                normal: None,
                emissive_tex: None,
                alpha_mode: 0,
                alpha_cutoff: 0.5,
            },
            &[],
        );

        let target_size = (ctx.config.width.max(1), ctx.config.height.max(1));
        let color_format = ctx.config.format;
        let depth_view = make_depth(device, target_size, sample_count);
        let msaa_view = make_msaa(device, target_size, color_format, sample_count);

        Self {
            pipeline,
            pipeline_blend,
            globals_buf,
            globals_group,
            material_bgl,
            sampler,
            defaults,
            depth_view,
            msaa_view,
            target_size,
            sample_count,
            color_format,
            meshes: Vec::new(),
            materials: Vec::new(),
            fallback_material,
            srgb_output: ctx.config.format.is_srgb() as u32 as f32,
        }
    }

    /// Replace the drawn geometry + materials with a freshly decoded preview model.
    pub fn upload(&mut self, ctx: &GpuContext, model: &CpuModel) {
        let device = &ctx.device;
        self.meshes = model
            .submeshes
            .iter()
            .map(|s| GpuMesh {
                vbuf: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("viewer-verts"),
                    contents: bytemuck::cast_slice(&s.vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                }),
                ibuf: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("viewer-indices"),
                    contents: bytemuck::cast_slice(&s.indices),
                    usage: wgpu::BufferUsages::INDEX,
                }),
                index_count: s.indices.len() as u32,
                material: s.material,
                blend: model
                    .materials
                    .get(s.material)
                    .is_some_and(|m| m.alpha_mode == 2),
                centroid: submesh_centroid(&s.vertices),
            })
            .collect();

        self.materials = model
            .materials
            .iter()
            .map(|m| {
                build_material(
                    device,
                    &ctx.queue,
                    &self.material_bgl,
                    &self.sampler,
                    &self.defaults,
                    m,
                    &model.textures,
                )
            })
            .collect();
    }

    pub fn has_model(&self) -> bool {
        !self.meshes.is_empty()
    }

    fn ensure_targets(&mut self, ctx: &GpuContext) {
        let size = (ctx.config.width.max(1), ctx.config.height.max(1));
        if size != self.target_size {
            self.depth_view = make_depth(&ctx.device, size, self.sample_count);
            self.msaa_view = make_msaa(&ctx.device, size, self.color_format, self.sample_count);
            self.target_size = size;
        }
    }

    /// The one draw loop: update globals → acquire → clear + draw each submesh with its material →
    /// present. Clears to the neutral viewer background even with no model.
    pub fn render(&mut self, ctx: &mut GpuContext, camera: &OrbitCamera) {
        self.ensure_targets(ctx);

        let globals = Globals {
            view_proj: camera.view_proj(ctx.aspect()).to_cols_array_2d(),
            camera_pos: camera.eye_pos().extend(1.0).to_array(),
            params: [self.srgb_output, 0.0, 0.0, 0.0],
        };
        ctx.queue
            .write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        let Some(frame) = ctx.acquire() else {
            return;
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        // Under MSAA the pass draws into the multisampled target and resolves into the swapchain
        // view; single-sample draws straight into the swapchain view.
        let (attachment, resolve_target) = match &self.msaa_view {
            Some(msaa) => (msaa, Some(&view)),
            None => (&view, None),
        };

        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("viewer-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("viewer-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: attachment,
                    resolve_target,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        // Dark neutral clear, matching the grid tile + headless CLEAR.
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.086,
                            g: 0.098,
                            b: 0.118,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            if !self.meshes.is_empty() {
                let materials = &self.materials;
                let fallback = &self.fallback_material;
                let draw = |pass: &mut wgpu::RenderPass, mesh: &GpuMesh| {
                    let mat = materials.get(mesh.material).unwrap_or(fallback);
                    pass.set_bind_group(1, mat, &[]);
                    pass.set_vertex_buffer(0, mesh.vbuf.slice(..));
                    pass.set_index_buffer(mesh.ibuf.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..mesh.index_count, 0, 0..1);
                };

                // Opaque + mask surfaces first (depth-writing), then blended glass back-to-front from
                // the current orbit camera so overlapping panes composite correctly.
                pass.set_bind_group(0, &self.globals_group, &[]);
                pass.set_pipeline(&self.pipeline);
                for mesh in self.meshes.iter().filter(|m| !m.blend) {
                    draw(&mut pass, mesh);
                }

                let mut blended: Vec<&GpuMesh> = self.meshes.iter().filter(|m| m.blend).collect();
                if !blended.is_empty() {
                    let eye = camera.eye_pos();
                    blended.sort_by(|a, b| {
                        let da = (a.centroid - eye).length_squared();
                        let db = (b.centroid - eye).length_squared();
                        db.total_cmp(&da) // farthest first (painter's order)
                    });
                    pass.set_pipeline(&self.pipeline_blend);
                    for mesh in blended {
                        draw(&mut pass, mesh);
                    }
                }
            }
        }
        ctx.queue.submit([encoder.finish()]);
        ctx.queue.present(frame);
    }
}

/// Upload a material's factors + (textured or default) maps into a bindable group. sRGB maps
/// (base/emissive) and linear maps (metallic-roughness/normal) get the matching texture format so
/// the shader samples in the right colour space, exactly as the headless renderer does.
fn build_material(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    defaults: &Defaults,
    m: &CpuMaterial,
    textures: &[CpuTexture],
) -> wgpu::BindGroup {
    let pick = |slot: Option<usize>, format: wgpu::TextureFormat, default: &wgpu::TextureView| {
        slot.and_then(|i| textures.get(i))
            .map(|t| upload_texture(device, queue, t, format))
            .unwrap_or_else(|| default.clone())
    };
    let base = pick(m.base, SRGB, &defaults.white_srgb);
    let mr = pick(m.mr, LINEAR, &defaults.white_linear);
    let normal = pick(m.normal, LINEAR, &defaults.normal);
    let emissive = pick(m.emissive_tex, SRGB, &defaults.black_srgb);

    let uniform = MaterialU {
        base_color: m.base_color,
        mr: [m.metallic, m.roughness, m.alpha_cutoff, m.alpha_mode as f32],
        emissive: [m.emissive[0], m.emissive[1], m.emissive[2], 0.0],
        flags: [
            m.base.is_some() as u32 as f32,
            m.mr.is_some() as u32 as f32,
            m.normal.is_some() as u32 as f32,
            m.emissive_tex.is_some() as u32 as f32,
        ],
    };
    let ubuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("viewer-material-uniform"),
        contents: bytemuck::bytes_of(&uniform),
        usage: wgpu::BufferUsages::UNIFORM,
    });

    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("viewer-material"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: ubuf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(&base),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(&mr),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::TextureView(&normal),
            },
            wgpu::BindGroupEntry {
                binding: 5,
                resource: wgpu::BindingResource::TextureView(&emissive),
            },
        ],
    })
}

/// Upload an RGBA8 texture and return a view. Single mip — the preview is already downscaled
/// server-side, and MSAA/mip chains stay on the headless path for WebGL2 compatibility.
fn upload_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    t: &CpuTexture,
    format: wgpu::TextureFormat,
) -> wgpu::TextureView {
    let (w, h) = (t.width.max(1), t.height.max(1));
    let size = wgpu::Extent3d {
        width: w,
        height: h,
        depth_or_array_layers: 1,
    };
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("viewer-texture"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &t.rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * 4),
            rows_per_image: Some(h),
        },
        size,
    );
    tex.create_view(&wgpu::TextureViewDescriptor::default())
}

/// Average vertex position of a submesh — a cheap centroid for back-to-front sorting of blended
/// (glass) surfaces against the orbit camera. Vertexless runs never reach here.
fn submesh_centroid(vertices: &[Vertex]) -> glam::Vec3 {
    if vertices.is_empty() {
        return glam::Vec3::ZERO;
    }
    let sum: glam::Vec3 = vertices.iter().map(|v| glam::Vec3::from(v.pos)).sum();
    sum / vertices.len() as f32
}

/// A 1×1 texture of a single RGBA colour — the neutral stand-in for an absent material map.
fn pixel(ctx: &GpuContext, rgba: [u8; 4], format: wgpu::TextureFormat) -> wgpu::TextureView {
    upload_texture(
        &ctx.device,
        &ctx.queue,
        &CpuTexture {
            rgba: rgba.to_vec(),
            width: 1,
            height: 1,
        },
        format,
    )
}

fn make_depth(device: &wgpu::Device, (w, h): (u32, u32), sample_count: u32) -> wgpu::TextureView {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("viewer-depth"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    tex.create_view(&wgpu::TextureViewDescriptor::default())
}

/// The multisampled colour target the pass resolves into the swapchain — `None` when single-sample
/// (WebGL2), where the pass draws straight to the swapchain view.
fn make_msaa(
    device: &wgpu::Device,
    (w, h): (u32, u32),
    format: wgpu::TextureFormat,
    sample_count: u32,
) -> Option<wgpu::TextureView> {
    if sample_count <= 1 {
        return None;
    }
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("viewer-msaa"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    Some(tex.create_view(&wgpu::TextureViewDescriptor::default()))
}
