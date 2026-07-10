//! Interactive 3D model preview for the native inspector.
//!
//! The engine already produces a self-contained `DMSH` preview blob (geometry + PBR materials +
//! textures) for the web island and the turntable thumbnail. Here we reuse that blob: parse it,
//! upload it to eframe's own wgpu device, and render an orbitable view **off-screen** into a texture
//! that egui then draws as an image. Rendering to our own target (with its own depth buffer)
//! sidesteps egui's depthless 2D pass, and re-rendering each frame makes the orbit live.
//!
//! Shading is the **same metallic-roughness PBR** the turntable thumbnail uses (`dam-render`'s
//! `pbr.wgsl`, ported here): GGX specular + a fixed 3-light studio rig + hemispheric ambient +
//! Reinhard tone-map, over base-colour / metallic-roughness / normal / emissive maps and factors.
//! The DMSH blob already carries every one of those (it feeds the thumbnail too), so the interactive
//! preview now reads identically to the grid thumbnail rather than as a flat, washed-out lambert.
//! Texture decode is fail-soft per map: a slot that won't decode falls back to its scalar factor.

use dam_api::AssetId;
use eframe::egui;
use eframe::wgpu;

/// The `DMSH` vertex, byte-identical to the blob's layout (`dam-render`/`dam-viewer`): 16 f32 = 64
/// bytes. All attributes are bound (pos/normal/tangent/uv/colour) so the PBR shader matches the
/// turntable thumbnail — tangents drive normal mapping, vertex colour tints albedo.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    pos: [f32; 3],
    normal: [f32; 3],
    tangent: [f32; 4],
    uv: [f32; 2],
    color: [f32; 4],
}

/// Camera + flags. `camera_pos` is the eye (for the PBR view vector); `params.x` = 1 when the
/// surface is sRGB (write linear, hardware encodes); `params.y` = lighting mode.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
    camera_pos: [f32; 4],
    params: [f32; 4],
}

/// Per-material uniform, mirroring `dam-render`'s `MaterialU`: base colour, metallic-roughness (+
/// alpha cutoff/mode), emissive, and `flags` = has-{base, mr, normal, emissive} texture.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct MaterialU {
    base_color: [f32; 4],
    mr: [f32; 4],       // x metallic, y roughness, z alpha cutoff, w alpha mode (0/1/2)
    emissive: [f32; 4], // rgb factor
    flags: [f32; 4],    // x has_base, y has_mr, z has_normal, w has_emissive
}

/// Axis-aligned bounds → orbit framing.
#[derive(Clone, Copy)]
struct Bounds {
    min: glam::Vec3,
    max: glam::Vec3,
}

impl Bounds {
    fn center(&self) -> glam::Vec3 {
        (self.min + self.max) * 0.5
    }
    fn radius(&self) -> f32 {
        ((self.max - self.min) * 0.5).length().max(1e-3)
    }
}

// Metallic-roughness PBR ported from `dam-render`'s `pbr.wgsl` so the interactive preview reads
// identically to the turntable thumbnail. Bindings mirror the render crate's material group (sampler,
// base, mr, normal, emissive); the one addition is `encode()` — the thumbnail always renders to an
// sRGB target, but eframe's surface may not, so we manually gamma-encode when it isn't.
const SHADER: &str = r#"
struct Globals { view_proj: mat4x4<f32>, camera_pos: vec4<f32>, params: vec4<f32> };
@group(0) @binding(0) var<uniform> g: Globals;

// flags.x has_base · y has_mr · z has_normal · w has_emissive
struct Material { base_color: vec4<f32>, mr: vec4<f32>, emissive: vec4<f32>, flags: vec4<f32> };
@group(1) @binding(0) var<uniform> mat: Material;
@group(1) @binding(1) var samp: sampler;
@group(1) @binding(2) var base_tex: texture_2d<f32>;
@group(1) @binding(3) var mr_tex: texture_2d<f32>;
@group(1) @binding(4) var normal_tex: texture_2d<f32>;
@group(1) @binding(5) var emissive_tex: texture_2d<f32>;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world_pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) tangent: vec3<f32>,
    @location(3) tangent_w: f32,
    @location(4) uv: vec2<f32>,
    @location(5) color: vec4<f32>,
};

@vertex
fn vs(
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) tangent: vec4<f32>,
    @location(3) uv: vec2<f32>,
    @location(4) color: vec4<f32>,
) -> VsOut {
    var o: VsOut;
    o.clip = g.view_proj * vec4<f32>(pos, 1.0);
    o.world_pos = pos;
    o.normal = normal;
    o.tangent = tangent.xyz;
    o.tangent_w = tangent.w;
    o.uv = uv;
    o.color = color;
    return o;
}

// params.x = surface is sRGB (write linear, hardware encodes); else gamma-encode by hand.
fn encode(c: vec3<f32>) -> vec4<f32> {
    if (g.params.x > 0.5) { return vec4<f32>(c, 1.0); }
    return vec4<f32>(pow(c, vec3<f32>(1.0 / 2.2)), 1.0);
}

const PI: f32 = 3.14159265359;

fn distribution_ggx(n_dot_h: f32, rough: f32) -> f32 {
    let a = rough * rough;
    let a2 = a * a;
    let d = n_dot_h * n_dot_h * (a2 - 1.0) + 1.0;
    return a2 / max(PI * d * d, 1e-5);
}

fn geometry_smith(n_dot_v: f32, n_dot_l: f32, rough: f32) -> f32 {
    let r = rough + 1.0;
    let k = (r * r) / 8.0;
    let gv = n_dot_v / (n_dot_v * (1.0 - k) + k);
    let gl = n_dot_l / (n_dot_l * (1.0 - k) + k);
    return gv * gl;
}

fn fresnel_schlick(cos_theta: f32, f0: vec3<f32>) -> vec3<f32> {
    return f0 + (vec3<f32>(1.0) - f0) * pow(clamp(1.0 - cos_theta, 0.0, 1.0), 5.0);
}

fn direct_light(
    l_dir: vec3<f32>, l_color: vec3<f32>,
    n: vec3<f32>, v: vec3<f32>, albedo: vec3<f32>, metallic: f32, rough: f32, f0: vec3<f32>,
) -> vec3<f32> {
    let l = normalize(l_dir);
    let h = normalize(v + l);
    let n_dot_l = max(dot(n, l), 0.0);
    if (n_dot_l <= 0.0) { return vec3<f32>(0.0); }
    let n_dot_v = max(dot(n, v), 1e-4);
    let n_dot_h = max(dot(n, h), 0.0);

    let ndf = distribution_ggx(n_dot_h, rough);
    let gg = geometry_smith(n_dot_v, n_dot_l, rough);
    let f = fresnel_schlick(max(dot(h, v), 0.0), f0);

    let spec = (ndf * gg * f) / max(4.0 * n_dot_v * n_dot_l, 1e-4);
    let kd = (vec3<f32>(1.0) - f) * (1.0 - metallic);
    let diffuse = kd * albedo / PI;
    return (diffuse + spec) * l_color * n_dot_l;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    var albedo = mat.base_color.rgb * in.color.rgb;
    if (mat.flags.x > 0.5) {
        albedo = albedo * textureSample(base_tex, samp, in.uv).rgb;
    }

    let mode = g.params.y;
    if (mode > 1.5) { return encode(albedo); } // flat / unlit

    var metallic = mat.mr.x;
    var rough = mat.mr.y;
    if (mat.flags.y > 0.5) {
        let mr = textureSample(mr_tex, samp, in.uv);
        rough = rough * mr.g;   // glTF packs roughness in G, metallic in B
        metallic = metallic * mr.b;
    }
    rough = clamp(rough, 0.04, 1.0);
    metallic = clamp(metallic, 0.0, 1.0);

    var n = normalize(in.normal);
    if (mat.flags.z > 0.5) {
        let t = normalize(in.tangent - n * dot(n, in.tangent));
        let b = cross(n, t) * in.tangent_w;
        let sampled = textureSample(normal_tex, samp, in.uv).xyz * 2.0 - 1.0;
        n = normalize(mat3x3<f32>(t, b, n) * sampled);
    }

    let v = normalize(g.camera_pos.xyz - in.world_pos);
    let f0 = mix(vec3<f32>(0.04), albedo, metallic);

    // Soft mode dims the direct rig and lifts ambient for a gentler, shadowless read; studio matches
    // the thumbnail's rig exactly.
    var key_s = 1.0; var amb_s = 1.0;
    if (mode > 0.5) { key_s = 0.45; amb_s = 1.7; }

    // Fixed studio rig (world space): warm key, cool fill, back rim.
    var lo = vec3<f32>(0.0);
    lo = lo + direct_light(vec3<f32>(0.5, 0.8, 0.6), vec3<f32>(3.0, 2.9, 2.7) * key_s, n, v, albedo, metallic, rough, f0);
    lo = lo + direct_light(vec3<f32>(-0.6, 0.3, 0.4), vec3<f32>(0.7, 0.8, 1.0) * key_s, n, v, albedo, metallic, rough, f0);
    lo = lo + direct_light(vec3<f32>(-0.2, 0.5, -0.9), vec3<f32>(1.1, 1.1, 1.4) * key_s, n, v, albedo, metallic, rough, f0);

    // Hemispheric ambient (cheap IBL substitute): sky above, darker ground below.
    let sky = vec3<f32>(0.40, 0.44, 0.52);
    let ground = vec3<f32>(0.11, 0.10, 0.10);
    let hemi = mix(ground, sky, clamp(n.y * 0.5 + 0.5, 0.0, 1.0));
    lo = lo + hemi * albedo * (1.0 - metallic * 0.6) * amb_s;

    // Emissive.
    if (mat.flags.w > 0.5) {
        lo = lo + mat.emissive.rgb * textureSample(emissive_tex, samp, in.uv).rgb;
    } else {
        lo = lo + mat.emissive.rgb;
    }

    // Reinhard tone-map, then encode to the (possibly non-sRGB) target.
    return encode(lo / (lo + vec3<f32>(1.0)));
}

// Wireframe overlay: a flat accent-coloured edge (line-list draw). Uses only group 0.
@fragment
fn fs_wire() -> @location(0) vec4<f32> {
    return encode(vec3<f32>(0.04, 0.50, 0.93)); // linear ≈ sky accent #38bdf8
}
"#;

const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// Off-screen render size (square). The inspector scales it down; this keeps it crisp when enlarged.
const SIZE: u32 = 512;
/// Sentinel for "material slot has no texture" (matches the `DMSH` writer).
const TEX_NONE: u32 = u32::MAX;

/// One drawable submesh: a range into the combined index buffer + the material bind group to bind.
struct GpuSub {
    index_start: u32,
    index_count: u32,
    mat: usize,
}

/// One uploaded model. `line_*` is a derived line-list edge buffer for the wireframe overlay.
struct GpuModel {
    vbuf: wgpu::Buffer,
    ibuf: wgpu::Buffer,
    line_ibuf: wgpu::Buffer,
    line_count: u32,
    bounds: Bounds,
    subs: Vec<GpuSub>,
    /// One bind group per material (uniform + base texture + sampler). Holds its resources alive.
    mat_bgs: Vec<wgpu::BindGroup>,
    /// Decoded base-colour textures, kept alive for the bind groups.
    #[allow(dead_code)]
    textures: Vec<wgpu::Texture>,
}

/// The inspector's interactive 3D preview renderer, bound to eframe's wgpu device.
pub struct Viewer3d {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    pipeline_wire: wgpu::RenderPipeline,
    globals_buf: wgpu::Buffer,
    globals_bg: wgpu::BindGroup,
    /// Layout for the per-material bind group (group 1).
    mat_bgl: wgpu::BindGroupLayout,
    /// Shared linear/repeat sampler for base-colour textures.
    sampler: wgpu::Sampler,
    /// 1×1 white texture bound for materials with no base map (the shader ignores it via `flags.x`).
    white_view: wgpu::TextureView,
    #[allow(dead_code)]
    white_tex: wgpu::Texture,
    /// Kept alive so its `color_view` (registered with egui) stays valid; not read directly.
    #[allow(dead_code)]
    color: wgpu::Texture,
    color_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,
    srgb: f32,
    tex_id: egui::TextureId,
    renderer: std::sync::Arc<egui::mutex::RwLock<eframe::egui_wgpu::Renderer>>,
    model: Option<GpuModel>,
    /// The asset the current model belongs to (so a new selection re-uploads).
    pub model_for: Option<AssetId>,
    // orbit state
    pub yaw: f32,
    pub pitch: f32,
    pub zoom: f32,
    // on-canvas controls (issue #65)
    pub auto_orbit: bool,
    pub wireframe: bool,
    pub lighting: u32, // 0 studio · 1 soft · 2 flat
}

impl Viewer3d {
    /// Build the renderer from eframe's wgpu render state (returns `None` if the GUI isn't on the
    /// wgpu backend, in which case the inspector falls back to the static thumbnail preview).
    pub fn new(rs: &eframe::egui_wgpu::RenderState) -> Self {
        let device = rs.device.clone();
        let queue = rs.queue.clone();

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("viewer3d"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewer3d-globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viewer3d-globals-bgl"),
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
        let globals_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("viewer3d-globals-bg"),
            layout: &globals_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buf.as_entire_binding(),
            }],
        });

        // Per-material bind group (mirrors dam-render): uniform + shared sampler + base / mr / normal
        // / emissive textures. Absent maps bind the 1×1 white texture; the shader gates each via `flags`.
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
        let mat_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viewer3d-material-bgl"),
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

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("viewer3d-sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let (white_tex, white_view) = make_white(&device, &queue);

        let color_format = rs.target_format;
        // pos@0→0, normal@12→1, tangent@24→2 (vec4), uv@40→3, colour@48→4 (vec4).
        let attrs = [
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: 0,
                shader_location: 0,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: 12,
                shader_location: 1,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 24,
                shader_location: 2,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 40,
                shader_location: 3,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 48,
                shader_location: 4,
            },
        ];
        let vbuf_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &attrs,
        };

        // Main (textured) pipeline uses groups 0 + 1; the wireframe pipeline only group 0.
        let layout_main = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("viewer3d-pl"),
            bind_group_layouts: &[&globals_bgl, &mat_bgl],
            push_constant_ranges: &[],
        });
        let layout_wire = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("viewer3d-pl-wire"),
            bind_group_layouts: &[&globals_bgl],
            push_constant_ranges: &[],
        });

        let color_target = wgpu::ColorTargetState {
            format: color_format,
            blend: Some(wgpu::BlendState::REPLACE),
            write_mask: wgpu::ColorWrites::ALL,
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("viewer3d-pipeline"),
            layout: Some(&layout_main),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: std::slice::from_ref(&vbuf_layout),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(color_target.clone())],
            }),
            primitive: wgpu::PrimitiveState {
                cull_mode: None, // game assets ship inconsistent winding
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::Less,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // Wireframe overlay: same vertex layout, line-list topology, flat accent fragment. Depth
        // test on but write off so it draws crisply over the shaded surface without z-fighting.
        let pipeline_wire = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("viewer3d-wire"),
            layout: Some(&layout_wire),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[vbuf_layout],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_wire"),
                compilation_options: Default::default(),
                targets: &[Some(color_target)],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::LineList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: false,
                depth_compare: wgpu::CompareFunction::LessEqual,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let (color, color_view) = make_color(&device, color_format);
        let depth_view = make_depth(&device);

        let renderer = rs.renderer.clone();
        let tex_id = renderer.write().register_native_texture(
            &device,
            &color_view,
            wgpu::FilterMode::Linear,
        );

        Self {
            device,
            queue,
            pipeline,
            pipeline_wire,
            globals_buf,
            globals_bg,
            mat_bgl,
            sampler,
            white_view,
            white_tex,
            color,
            color_view,
            depth_view,
            srgb: color_format.is_srgb() as u32 as f32,
            tex_id,
            renderer,
            model: None,
            model_for: None,
            yaw: std::f32::consts::FRAC_PI_4,
            pitch: 0.5,
            zoom: 1.0,
            auto_orbit: false,
            wireframe: false,
            lighting: 0,
        }
    }

    /// Parse + upload a `DMSH` blob (geometry + materials + base-colour textures) for `id`, resetting
    /// the orbit pose. Fails only if the geometry itself is unusable — a bad texture degrades to its
    /// flat base-colour factor rather than aborting.
    pub fn set_model(&mut self, id: AssetId, bytes: &[u8]) -> Result<(), String> {
        let dmsh = parse_dmsh(bytes)?;
        let vbuf = create_buffer_init(
            &self.device,
            "viewer3d-verts",
            bytemuck::cast_slice(&dmsh.verts),
            wgpu::BufferUsages::VERTEX,
        );
        let ibuf = create_buffer_init(
            &self.device,
            "viewer3d-indices",
            bytemuck::cast_slice(&dmsh.indices),
            wgpu::BufferUsages::INDEX,
        );
        // Wireframe edge buffer: each triangle's three edges, de-duplicated (low→high keyed).
        let mut edges: std::collections::HashSet<(u32, u32)> =
            std::collections::HashSet::with_capacity(dmsh.indices.len());
        for tri in dmsh.indices.chunks_exact(3) {
            for &(a, b) in &[(tri[0], tri[1]), (tri[1], tri[2]), (tri[2], tri[0])] {
                edges.insert(if a <= b { (a, b) } else { (b, a) });
            }
        }
        let line_indices: Vec<u32> = edges.into_iter().flat_map(|(a, b)| [a, b]).collect();
        let line_ibuf = create_buffer_init(
            &self.device,
            "viewer3d-line-indices",
            bytemuck::cast_slice(&line_indices),
            wgpu::BufferUsages::INDEX,
        );

        // One bind group per material, decoding its four maps in the right colour space (base/emissive
        // sRGB, mr/normal linear), memoised so a shared map isn't decoded twice.
        let raw_textures: Vec<wgpu::Texture> = Vec::new(); // views hold their textures alive via wgpu
        let mut tex_cache: std::collections::HashMap<(usize, bool), Option<wgpu::TextureView>> =
            std::collections::HashMap::new();
        let mut mat_bgs: Vec<wgpu::BindGroup> = Vec::with_capacity(dmsh.materials.len() + 1);
        for m in &dmsh.materials {
            mat_bgs.push(self.build_material_bg(m, &dmsh.textures, &mut tex_cache));
        }
        // A default material for submeshes whose material index is missing/out of range (dam-render's
        // fallback: white, dielectric, mid-rough).
        let default_mat = mat_bgs.len();
        mat_bgs.push(self.build_material_bg(&Mat::DEFAULT, &dmsh.textures, &mut tex_cache));

        let subs: Vec<GpuSub> = dmsh
            .subs
            .iter()
            .map(|s| GpuSub {
                index_start: s.index_start,
                index_count: s.index_count,
                mat: if s.material < default_mat {
                    s.material
                } else {
                    default_mat
                },
            })
            .collect();

        self.model = Some(GpuModel {
            vbuf,
            ibuf,
            line_ibuf,
            line_count: line_indices.len() as u32,
            bounds: dmsh.bounds,
            subs,
            mat_bgs,
            textures: raw_textures,
        });
        self.model_for = Some(id);
        self.reset_pose();
        Ok(())
    }

    /// Build one material's bind group: the PBR uniform + shared sampler + its four maps (each in the
    /// right colour space, `None` slots bound to the 1×1 white texture and gated off via `flags`).
    fn build_material_bg(
        &self,
        m: &Mat,
        pngs: &[Vec<u8>],
        cache: &mut std::collections::HashMap<(usize, bool), Option<wgpu::TextureView>>,
    ) -> wgpu::BindGroup {
        let base = slot_view(&self.device, &self.queue, pngs, cache, m.base_tex, true);
        let mr = slot_view(&self.device, &self.queue, pngs, cache, m.mr_tex, false);
        let normal = slot_view(&self.device, &self.queue, pngs, cache, m.normal_tex, false);
        let emissive = slot_view(&self.device, &self.queue, pngs, cache, m.emissive_tex, true);
        let u = MaterialU {
            base_color: m.base_color,
            mr: [m.metallic, m.roughness, 0.0, 0.0],
            emissive: [m.emissive[0], m.emissive[1], m.emissive[2], 0.0],
            flags: [
                base.is_some() as u32 as f32,
                mr.is_some() as u32 as f32,
                normal.is_some() as u32 as f32,
                emissive.is_some() as u32 as f32,
            ],
        };
        let ubuf = create_buffer_init(
            &self.device,
            "viewer3d-material",
            bytemuck::bytes_of(&u),
            wgpu::BufferUsages::UNIFORM,
        );
        let white = &self.white_view;
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("viewer3d-material-bg"),
            layout: &self.mat_bgl,
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
                    resource: wgpu::BindingResource::TextureView(base.as_ref().unwrap_or(white)),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(mr.as_ref().unwrap_or(white)),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(normal.as_ref().unwrap_or(white)),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(
                        emissive.as_ref().unwrap_or(white),
                    ),
                },
            ],
        })
    }

    /// Render the current model at the current orbit pose into the off-screen texture. Returns the
    /// egui texture id to draw. No-op-safe when no model is loaded (renders the clear background).
    pub fn render(&mut self) -> egui::TextureId {
        let (view_proj, eye) = self.camera();
        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            camera_pos: [eye.x, eye.y, eye.z, 1.0],
            params: [self.srgb, self.lighting as f32, 0.0, 0.0],
        };
        self.queue
            .write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("viewer3d-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("viewer3d-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.color_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
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
            });
            if let Some(m) = &self.model {
                pass.set_bind_group(0, &self.globals_bg, &[]);
                pass.set_vertex_buffer(0, m.vbuf.slice(..));
                if !self.wireframe {
                    // Solid shaded pass — one draw per submesh with its material bound.
                    pass.set_pipeline(&self.pipeline);
                    pass.set_index_buffer(m.ibuf.slice(..), wgpu::IndexFormat::Uint32);
                    for s in &m.subs {
                        if let Some(bg) = m.mat_bgs.get(s.mat) {
                            pass.set_bind_group(1, bg, &[]);
                            pass.draw_indexed(
                                s.index_start..s.index_start + s.index_count,
                                0,
                                0..1,
                            );
                        }
                    }
                } else {
                    pass.set_pipeline(&self.pipeline_wire);
                    pass.set_index_buffer(m.line_ibuf.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..m.line_count, 0, 0..1);
                }
            }
        }
        self.queue.submit([encoder.finish()]);
        self.tex_id
    }

    /// View-projection matrix + the world-space eye position (the PBR shader needs the eye for its
    /// view vector / specular).
    fn camera(&self) -> (glam::Mat4, glam::Vec3) {
        let (center, radius) = match &self.model {
            Some(m) => (m.bounds.center(), m.bounds.radius()),
            None => (glam::Vec3::ZERO, 1.0),
        };
        let dist = radius * 2.6 * self.zoom;
        let dir = glam::Vec3::new(
            self.pitch.cos() * self.yaw.sin(),
            self.pitch.sin(),
            self.pitch.cos() * self.yaw.cos(),
        );
        let eye = center + dir * dist;
        let view = glam::Mat4::look_at_rh(eye, center, glam::Vec3::Y);
        let proj = glam::Mat4::perspective_rh(
            45f32.to_radians(),
            1.0,
            (radius * 0.02).max(1e-3),
            radius * 100.0,
        );
        (proj * view, eye)
    }

    /// Apply a pointer drag (orbit) and scroll (zoom).
    pub fn orbit(&mut self, drag: egui::Vec2, scroll: f32) {
        const LIMIT: f32 = std::f32::consts::FRAC_PI_2 - 0.05;
        self.yaw += drag.x * 0.01;
        self.pitch = (self.pitch + drag.y * 0.01).clamp(-LIMIT, LIMIT);
        if scroll != 0.0 {
            self.zoom = (self.zoom * (scroll * -0.0015).exp()).clamp(0.1, 10.0);
        }
    }

    /// Advance the auto-orbit spin by `dt` seconds (no-op unless `auto_orbit` is set).
    pub fn tick(&mut self, dt: f32) {
        if self.auto_orbit {
            self.yaw += dt * 0.6;
        }
    }

    /// Restore the default framing pose (used by the reset button).
    pub fn reset_pose(&mut self) {
        self.yaw = std::f32::consts::FRAC_PI_4;
        self.pitch = 0.5;
        self.zoom = 1.0;
    }
}

impl Drop for Viewer3d {
    fn drop(&mut self) {
        self.renderer.write().free_texture(&self.tex_id);
    }
}

fn make_color(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
) -> (wgpu::Texture, wgpu::TextureView) {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("viewer3d-color"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    (tex, view)
}

fn make_depth(device: &wgpu::Device) -> wgpu::TextureView {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("viewer3d-depth"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    tex.create_view(&wgpu::TextureViewDescriptor::default())
}

/// A 1×1 white texture bound for materials with no base map (the shader ignores it via `flags.x`).
fn make_white(device: &wgpu::Device, queue: &wgpu::Queue) -> (wgpu::Texture, wgpu::TextureView) {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("viewer3d-white"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
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
        &[255, 255, 255, 255],
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
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    (tex, view)
}

/// Decode a PNG material map and upload it. Colour maps (base, emissive) are sRGB; data maps
/// (metallic-roughness, normal) are linear (`srgb = false`) so their values aren't gamma-warped.
/// Returns `None` on any decode error (the material then falls back to its scalar factor).
fn decode_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    png: &[u8],
    srgb: bool,
) -> Option<wgpu::TextureView> {
    let img = image::load_from_memory(png).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let format = if srgb {
        wgpu::TextureFormat::Rgba8UnormSrgb
    } else {
        wgpu::TextureFormat::Rgba8Unorm
    };
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("viewer3d-material-map"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
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
            texture: &tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &img,
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
    Some(tex.create_view(&wgpu::TextureViewDescriptor::default()))
}

fn create_buffer_init(
    device: &wgpu::Device,
    label: &str,
    contents: &[u8],
    usage: wgpu::BufferUsages,
) -> wgpu::Buffer {
    use wgpu::util::DeviceExt;
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents,
        usage,
    })
}

/// A parsed material: PBR factors + optional texture slots (indices into the DMSH texture table).
struct Mat {
    base_color: [f32; 4],
    metallic: f32,
    roughness: f32,
    emissive: [f32; 3],
    base_tex: Option<usize>,
    mr_tex: Option<usize>,
    normal_tex: Option<usize>,
    emissive_tex: Option<usize>,
}

impl Mat {
    /// Fallback for submeshes with no valid material (matches `dam-render`'s default: white,
    /// dielectric, mid-rough).
    const DEFAULT: Mat = Mat {
        base_color: [1.0, 1.0, 1.0, 1.0],
        metallic: 0.0,
        roughness: 0.6,
        emissive: [0.0, 0.0, 0.0],
        base_tex: None,
        mr_tex: None,
        normal_tex: None,
        emissive_tex: None,
    };
}

/// Decode a material's texture slot as a view, memoised by (index, colour-space) so a map shared by
/// several materials is decoded once. `None` slot or a decode failure → `None`; the shader then
/// falls back to the material's scalar factor.
fn slot_view(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pngs: &[Vec<u8>],
    cache: &mut std::collections::HashMap<(usize, bool), Option<wgpu::TextureView>>,
    idx: Option<usize>,
    srgb: bool,
) -> Option<wgpu::TextureView> {
    let idx = idx?;
    cache
        .entry((idx, srgb))
        .or_insert_with(|| {
            pngs.get(idx)
                .and_then(|png| decode_texture(device, queue, png, srgb))
        })
        .clone()
}

/// A parsed submesh range into the combined index buffer + its material index.
struct Sub {
    index_start: u32,
    index_count: u32,
    material: usize,
}

/// The parsed `DMSH` contents the uploader needs.
struct Dmsh {
    verts: Vec<Vertex>,
    indices: Vec<u32>,
    subs: Vec<Sub>,
    materials: Vec<Mat>,
    textures: Vec<Vec<u8>>, // PNG blobs
    bounds: Bounds,
}

/// A bounded little-endian parse of the `DMSH` v2 blob (mirrors dam-viewer's reader): bounds, the
/// texture table (PNG blobs), materials (base colour + base-texture slot), and submeshes flattened
/// into one vertex/index buffer with per-submesh index ranges.
fn parse_dmsh(bytes: &[u8]) -> Result<Dmsh, String> {
    let mut p = 0usize;
    let take = |p: &mut usize, n: usize| -> Result<&[u8], String> {
        let end = p.checked_add(n).ok_or("DMSH length overflow")?;
        let s = bytes.get(*p..end).ok_or("unexpected end of DMSH blob")?;
        *p = end;
        Ok(s)
    };
    let u32r = |p: &mut usize| -> Result<u32, String> {
        let s = take(p, 4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    };
    let f32r = |p: &mut usize| -> Result<f32, String> { Ok(f32::from_bits(u32r(p)?)) };

    if take(&mut p, 4)? != b"DMSH" {
        return Err("not a DMSH preview blob".into());
    }
    if u32r(&mut p)? != 2 {
        return Err("unsupported DMSH version".into());
    }
    let min = glam::Vec3::new(f32r(&mut p)?, f32r(&mut p)?, f32r(&mut p)?);
    let max = glam::Vec3::new(f32r(&mut p)?, f32r(&mut p)?, f32r(&mut p)?);

    // Texture table (PNG runs).
    let n_tex = u32r(&mut p)?;
    let mut textures: Vec<Vec<u8>> = Vec::with_capacity(n_tex as usize);
    for _ in 0..n_tex {
        let len = u32r(&mut p)? as usize;
        textures.push(take(&mut p, len)?.to_vec());
    }

    // Materials: base_color(4f) metallic(f) roughness(f) emissive(3f) tex_base(u) tex_mr(u)
    // tex_normal(u) tex_emissive(u) alpha_mode(u) alpha_cutoff(f). The PBR shader uses everything but
    // alpha (the preview renders opaque); `slot` maps u32::MAX → None.
    let slot = |i: u32| (i != TEX_NONE).then_some(i as usize);
    let n_mat = u32r(&mut p)?;
    let mut materials: Vec<Mat> = Vec::with_capacity(n_mat as usize);
    for _ in 0..n_mat {
        let base_color = [f32r(&mut p)?, f32r(&mut p)?, f32r(&mut p)?, f32r(&mut p)?];
        let metallic = f32r(&mut p)?;
        let roughness = f32r(&mut p)?;
        let emissive = [f32r(&mut p)?, f32r(&mut p)?, f32r(&mut p)?];
        let tex_base = u32r(&mut p)?;
        let tex_mr = u32r(&mut p)?;
        let tex_normal = u32r(&mut p)?;
        let tex_emissive = u32r(&mut p)?;
        let _alpha_mode = u32r(&mut p)?;
        let _alpha_cutoff = f32r(&mut p)?;
        materials.push(Mat {
            base_color,
            metallic,
            roughness,
            emissive,
            base_tex: slot(tex_base),
            mr_tex: slot(tex_mr),
            normal_tex: slot(tex_normal),
            emissive_tex: slot(tex_emissive),
        });
    }

    let n_sub = u32r(&mut p)?;
    let mut verts: Vec<Vertex> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut subs: Vec<Sub> = Vec::with_capacity(n_sub as usize);
    for _ in 0..n_sub {
        let material = u32r(&mut p)? as usize;
        let n_vert = u32r(&mut p)? as usize;
        let vbytes = take(&mut p, n_vert * std::mem::size_of::<Vertex>())?;
        let sub_verts = bytemuck::pod_collect_to_vec::<u8, Vertex>(vbytes);
        let base = verts.len() as u32;
        let n_idx = u32r(&mut p)? as usize;
        let ibytes = take(&mut p, n_idx * 4)?;
        let index_start = indices.len() as u32;
        for idx in bytemuck::pod_collect_to_vec::<u8, u32>(ibytes) {
            indices.push(base + idx);
        }
        subs.push(Sub {
            index_start,
            index_count: n_idx as u32,
            material,
        });
        verts.extend(sub_verts);
    }
    if verts.is_empty() {
        return Err("preview blob has no geometry".into());
    }
    Ok(Dmsh {
        verts,
        indices,
        subs,
        materials,
        textures,
        bounds: Bounds { min, max },
    })
}
