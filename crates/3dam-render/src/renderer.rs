//! The wgpu renderer: adapter fallback ladder, PBR pipeline, per-material textured draws with
//! mipmapped textures, offscreen depth+MSAA render, readback → PNG.
//!
//! Ports the headless path validated by `spikes/headless-render/` (ADR 0001) and extends it to a
//! full metallic-roughness PBR render of multi-material models (ADR 0011). The device is created
//! once and shared — device init is the dominant cost, so it must not happen per thumbnail.

use crate::camera;
use crate::model::{Material, Model, TexImage, Vertex};
use crate::RenderError;
use wgpu::util::DeviceExt;

const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const SRGB: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const LINEAR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Supersampling factor: rasterise the turntable at this multiple of the requested edge, then
/// downsample. Anti-aliases silhouettes independently of MSAA support (software adapters get 1×).
const SSAA: u32 = 2;

/// Dark neutral clear so the model reads on the grid's dark tiles (design tokens, `--color-bg`).
const CLEAR: wgpu::Color = wgpu::Color {
    r: 0.086,
    g: 0.098,
    b: 0.118,
    a: 1.0,
};

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
    camera_pos: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct MaterialU {
    base_color: [f32; 4],
    mr: [f32; 4],
    emissive: [f32; 4],
    flags: [f32; 4],
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pbr_pipeline: wgpu::RenderPipeline,
    mip_pipeline_srgb: wgpu::RenderPipeline,
    mip_pipeline_linear: wgpu::RenderPipeline,
    globals_bgl: wgpu::BindGroupLayout,
    material_bgl: wgpu::BindGroupLayout,
    mip_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    mip_sampler: wgpu::Sampler,
    default_white_srgb: wgpu::TextureView,
    default_white_linear: wgpu::TextureView,
    default_normal: wgpu::TextureView,
    default_black_srgb: wgpu::TextureView,
    sample_count: u32,
    max_texture_dim: u32,
    info: String,
}

impl Renderer {
    pub fn info(&self) -> &str {
        &self.info
    }

    pub async fn new() -> Result<Self, RenderError> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle().with_env());

        let mut adapter = None;
        for force_fallback in [false, true] {
            if let Ok(a) = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    force_fallback_adapter: force_fallback,
                    compatible_surface: None,
                    apply_limit_buckets: false,
                })
                .await
            {
                adapter = Some(a);
                break;
            }
        }
        let adapter = adapter.ok_or(RenderError::NoAdapter)?;
        let ai = adapter.get_info();
        let is_software = ai.device_type == wgpu::DeviceType::Cpu;

        let features = adapter.get_texture_format_features(COLOR_FORMAT);
        let sample_count = if features.flags.sample_count_supported(4) {
            4
        } else {
            1
        };

        // Stay on downlevel_defaults for broad compatibility, but raise the 2D texture-dimension
        // cap (2048 by default) to whatever this adapter actually supports — real GPUs handle 8k+
        // material maps, and clamping them to 2048 would needlessly blur thumbnails.
        let mut required_limits = wgpu::Limits::downlevel_defaults();
        required_limits.max_texture_dimension_2d = adapter.limits().max_texture_dimension_2d;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("dam-render-device"),
                required_limits,
                ..Default::default()
            })
            .await
            .map_err(|e| RenderError::Device(e.to_string()))?;
        let max_texture_dim = device.limits().max_texture_dimension_2d;

        // ── bind group layouts ───────────────────────────────────────────────
        let globals_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals"),
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
            label: Some("material"),
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

        let mip_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("mip"),
            entries: &[
                tex_entry(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        // ── pipelines ────────────────────────────────────────────────────────
        let pbr_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pbr"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/pbr.wgsl").into()),
        });
        let pbr_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pbr-layout"),
            bind_group_layouts: &[Some(&globals_bgl), Some(&material_bgl)],
            immediate_size: 0,
        });
        let pbr_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pbr-pipeline"),
            layout: Some(&pbr_layout),
            vertex: wgpu::VertexState {
                module: &pbr_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x3, 1 => Float32x3, 2 => Float32x4, 3 => Float32x2, 4 => Float32x4
                    ],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &pbr_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: COLOR_FORMAT,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                cull_mode: None, // two-sided: game assets often have inconsistent winding
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState {
                count: sample_count,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview_mask: None,
            cache: None,
        });

        let mip_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/blit.wgsl").into()),
        });
        let mip_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("mip-layout"),
            bind_group_layouts: &[Some(&mip_bgl)],
            immediate_size: 0,
        });
        let make_mip_pipeline = |format: wgpu::TextureFormat| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("mip-pipeline"),
                layout: Some(&mip_layout),
                vertex: wgpu::VertexState {
                    module: &mip_shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &mip_shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::REPLACE),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let mip_pipeline_srgb = make_mip_pipeline(SRGB);
        let mip_pipeline_linear = make_mip_pipeline(LINEAR);

        // ── samplers + default textures ──────────────────────────────────────
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("material-sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let mip_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("mip-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let default_white_srgb = pixel_texture(&device, &queue, [255, 255, 255, 255], SRGB);
        let default_white_linear = pixel_texture(&device, &queue, [255, 255, 255, 255], LINEAR);
        let default_normal = pixel_texture(&device, &queue, [128, 128, 255, 255], LINEAR);
        let default_black_srgb = pixel_texture(&device, &queue, [0, 0, 0, 255], SRGB);

        let info = format!(
            "{} ({:?}, software={}, msaa={}x)",
            ai.name, ai.backend, is_software, sample_count
        );

        Ok(Renderer {
            device,
            queue,
            pbr_pipeline,
            mip_pipeline_srgb,
            mip_pipeline_linear,
            globals_bgl,
            material_bgl,
            mip_bgl,
            sampler,
            mip_sampler,
            default_white_srgb,
            default_white_linear,
            default_normal,
            default_black_srgb,
            sample_count,
            max_texture_dim,
            info,
        })
    }

    /// Render a model to a square `size`×`size` PNG from the canonical turntable angle.
    pub fn render_png(&self, model: &Model, size: u32) -> Result<Vec<u8>, RenderError> {
        let size = size.max(16);
        // Supersample: rasterise at `SSAA`× the requested edge and downsample the readback. This
        // gives clean anti-aliased silhouettes even on software adapters with no MSAA, and keeps the
        // high-frequency texture detail a single-sample thumbnail raster would otherwise alias away.
        let render_size = size.saturating_mul(SSAA).min(self.max_texture_dim);
        let device = &self.device;

        // Materials → GPU (textures with mips + factor uniform + bind group).
        let material_groups: Vec<wgpu::BindGroup> = model
            .materials
            .iter()
            .map(|m| self.build_material(m))
            .collect();
        // A model can reference a material index with no material entry — supply a default.
        let fallback_material = self.build_material(&Material {
            base_color: [0.8, 0.8, 0.82, 1.0],
            metallic: 0.0,
            roughness: 0.6,
            emissive: [0.0; 3],
            base_color_tex: None,
            mr_tex: None,
            normal_tex: None,
            emissive_tex: None,
        });

        // Submeshes → vertex/index buffers.
        struct GpuMesh {
            vbuf: wgpu::Buffer,
            ibuf: wgpu::Buffer,
            count: u32,
            material: usize,
        }
        let meshes: Vec<GpuMesh> = model
            .submeshes
            .iter()
            .map(|s| GpuMesh {
                vbuf: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("verts"),
                    contents: bytemuck::cast_slice(&s.vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                }),
                ibuf: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("indices"),
                    contents: bytemuck::cast_slice(&s.indices),
                    usage: wgpu::BufferUsages::INDEX,
                }),
                count: s.indices.len() as u32,
                material: s.material,
            })
            .collect();

        // Globals (camera).
        let cam = camera::frame(&model.bounds, 1.0);
        let globals = Globals {
            view_proj: cam.view_proj.to_cols_array_2d(),
            camera_pos: [cam.eye.x, cam.eye.y, cam.eye.z, 1.0],
        };
        let gbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("globals"),
            contents: bytemuck::bytes_of(&globals),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let globals_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals"),
            layout: &self.globals_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: gbuf.as_entire_binding(),
            }],
        });

        // Render targets (at the supersampled resolution).
        let extent = wgpu::Extent3d {
            width: render_size,
            height: render_size,
            depth_or_array_layers: 1,
        };
        let resolve = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("resolve"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: COLOR_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let resolve_view = resolve.create_view(&wgpu::TextureViewDescriptor::default());
        let msaa_view = (self.sample_count > 1).then(|| {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("msaa"),
                    size: extent,
                    mip_level_count: 1,
                    sample_count: self.sample_count,
                    dimension: wgpu::TextureDimension::D2,
                    format: COLOR_FORMAT,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor::default())
        });
        let depth_view = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("depth"),
                size: extent,
                mip_level_count: 1,
                sample_count: self.sample_count,
                dimension: wgpu::TextureDimension::D2,
                format: DEPTH_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default());

        let (color_view, resolve_target) = match &msaa_view {
            Some(m) => (m, Some(&resolve_view)),
            None => (&resolve_view, None),
        };

        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("enc") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: color_view,
                    resolve_target,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(CLEAR),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pbr_pipeline);
            pass.set_bind_group(0, &globals_group, &[]);
            for mesh in &meshes {
                let mat = material_groups
                    .get(mesh.material)
                    .unwrap_or(&fallback_material);
                pass.set_bind_group(1, mat, &[]);
                pass.set_vertex_buffer(0, mesh.vbuf.slice(..));
                pass.set_index_buffer(mesh.ibuf.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..mesh.count, 0, 0..1);
            }
        }

        let pixels = self.readback_rgba(&mut encoder, &resolve, render_size)?;
        let pixels = if render_size != size {
            downscale_rgba(&pixels, render_size, size)
        } else {
            pixels
        };
        encode_png(&pixels, size)
    }

    /// Upload a material's textures (with mipmaps) and factors into a bindable group.
    fn build_material(&self, m: &Material) -> wgpu::BindGroup {
        let base = self.upload_or_default(&m.base_color_tex, SRGB, &self.default_white_srgb);
        let mr = self.upload_or_default(&m.mr_tex, LINEAR, &self.default_white_linear);
        let normal = self.upload_or_default(&m.normal_tex, LINEAR, &self.default_normal);
        let emissive = self.upload_or_default(&m.emissive_tex, SRGB, &self.default_black_srgb);

        let uniform = MaterialU {
            base_color: m.base_color,
            mr: [m.metallic, m.roughness, 0.0, 0.0],
            emissive: [m.emissive[0], m.emissive[1], m.emissive[2], 0.0],
            flags: [
                m.base_color_tex.is_some() as u32 as f32,
                m.mr_tex.is_some() as u32 as f32,
                m.normal_tex.is_some() as u32 as f32,
                m.emissive_tex.is_some() as u32 as f32,
            ],
        };
        let ubuf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("material-uniform"),
                contents: bytemuck::bytes_of(&uniform),
                usage: wgpu::BufferUsages::UNIFORM,
            });

        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("material"),
            layout: &self.material_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: ubuf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
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

    fn upload_or_default(
        &self,
        tex: &Option<TexImage>,
        format: wgpu::TextureFormat,
        default: &wgpu::TextureView,
    ) -> wgpu::TextureView {
        match tex {
            Some(img) => self.upload_texture(img, format),
            None => default.clone(),
        }
    }

    /// Upload an RGBA8 texture and generate its full mip chain via the blit pipeline.
    fn upload_texture(&self, img: &TexImage, format: wgpu::TextureFormat) -> wgpu::TextureView {
        // Fail-soft: a map larger than the device's max 2D texture dimension (as low as 2048 on
        // software/downlevel adapters) would fail texture creation — downscale it to fit instead.
        let clamped = clamp_texture(img, self.max_texture_dim);
        let img = clamped.as_ref().unwrap_or(img);
        let (w, h) = (img.width.max(1), img.height.max(1));
        let mip_count = 32 - (w.max(h)).leading_zeros();
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("material-texture"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: mip_count,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &img.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );

        if mip_count > 1 {
            let pipeline = if format == SRGB {
                &self.mip_pipeline_srgb
            } else {
                &self.mip_pipeline_linear
            };
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("mipgen"),
                });
            for level in 1..mip_count {
                let src = texture.create_view(&wgpu::TextureViewDescriptor {
                    base_mip_level: level - 1,
                    mip_level_count: Some(1),
                    ..Default::default()
                });
                let dst = texture.create_view(&wgpu::TextureViewDescriptor {
                    base_mip_level: level,
                    mip_level_count: Some(1),
                    ..Default::default()
                });
                let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("mip"),
                    layout: &self.mip_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&src),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&self.mip_sampler),
                        },
                    ],
                });
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("mip"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &dst,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, &group, &[]);
                pass.draw(0..3, 0..1);
            }
            self.queue.submit([encoder.finish()]);
        }

        texture.create_view(&wgpu::TextureViewDescriptor::default())
    }

    /// Copy the rendered colour target back to CPU as tightly-packed RGBA8 (`size`×`size`).
    fn readback_rgba(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        resolve: &wgpu::Texture,
        size: u32,
    ) -> Result<Vec<u8>, RenderError> {
        let unpadded_bpr = size * 4;
        let padded_bpr = align_up(unpadded_bpr, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded_bpr * size) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: resolve,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bpr),
                    rows_per_image: Some(size),
                },
            },
            wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
        );

        let cmd = std::mem::replace(
            encoder,
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None }),
        );
        self.queue.submit([cmd.finish()]);

        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        rx.recv()
            .map_err(|e| RenderError::Readback(e.to_string()))?
            .map_err(|e| RenderError::Readback(e.to_string()))?;

        let data = slice
            .get_mapped_range()
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        let mut pixels = Vec::with_capacity((unpadded_bpr * size) as usize);
        for row in 0..size {
            let start = (row * padded_bpr) as usize;
            pixels.extend_from_slice(&data[start..start + unpadded_bpr as usize]);
        }
        drop(data);
        readback.unmap();

        Ok(pixels)
    }
}

/// Create a single-pixel texture (defaults for absent material maps).
fn pixel_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    rgba: [u8; 4],
    format: wgpu::TextureFormat,
) -> wgpu::TextureView {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("default-pixel"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4),
            rows_per_image: Some(1),
        },
        wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
    );
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

/// Downscale a texture to fit within `max_dim` on both axes, preserving aspect. Returns `None`
/// when it already fits (the common case) so the caller uploads the original without copying.
fn clamp_texture(img: &TexImage, max_dim: u32) -> Option<TexImage> {
    let (w, h) = (img.width.max(1), img.height.max(1));
    if w <= max_dim && h <= max_dim {
        return None;
    }
    let scale = max_dim as f32 / w.max(h) as f32;
    let nw = ((w as f32 * scale) as u32).clamp(1, max_dim);
    let nh = ((h as f32 * scale) as u32).clamp(1, max_dim);
    let src = image::RgbaImage::from_raw(w, h, img.rgba.clone())?;
    let resized = image::imageops::resize(&src, nw, nh, image::imageops::FilterType::Triangle);
    Some(TexImage {
        rgba: resized.into_raw(),
        width: nw,
        height: nh,
    })
}

fn align_up(n: u32, align: u32) -> u32 {
    n.div_ceil(align) * align
}

fn encode_png(rgba: &[u8], size: u32) -> Result<Vec<u8>, RenderError> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(rgba, size, size, image::ExtendedColorType::Rgba8)
        .map_err(|e| RenderError::Encode(e.to_string()))?;
    Ok(out)
}

/// Downsample a square RGBA8 buffer from `from`×`from` to `to`×`to` (Lanczos3 for a crisp result).
/// The supersample resolve — falls back to the source on the (unreachable) reconstruction failure.
fn downscale_rgba(pixels: &[u8], from: u32, to: u32) -> Vec<u8> {
    match image::RgbaImage::from_raw(from, from, pixels.to_vec()) {
        Some(img) => {
            image::imageops::resize(&img, to, to, image::imageops::FilterType::Lanczos3).into_raw()
        }
        None => pixels.to_vec(),
    }
}
