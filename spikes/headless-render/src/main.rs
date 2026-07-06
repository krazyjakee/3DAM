//! Spike: headless wgpu render-to-PNG, and the software-raster fallback.
//!
//! Validates the load-bearing claim in ADR 0001 / tech-spec 06 §4: that a single
//! no-per-OS-branch path can request an adapter with NO surface, render to an offscreen
//! texture, read it back, and encode a PNG — and that when no real GPU is present a software
//! rasteriser (Mesa lavapipe) still produces a frame with our pipeline.
//!
//! Usage:
//!   spike-headless-render [OUT.png] [--fallback]
//!     --fallback  => request with force_fallback_adapter (fallback-ladder rung 2)
//!   To force lavapipe directly (rung 3), run with:
//!     VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json spike-headless-render out.png

use std::borrow::Cow;

const WIDTH: u32 = 512;
const HEIGHT: u32 = 512;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    pos: [f32; 2],
    color: [f32; 3],
}

const SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec3<f32>,
};

@vertex
fn vs_main(@location(0) pos: vec2<f32>, @location(1) color: vec3<f32>) -> VsOut {
    var out: VsOut;
    out.pos = vec4<f32>(pos, 0.0, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, 1.0);
}
"#;

fn align_up(n: u32, align: u32) -> u32 {
    (n + align - 1) / align * align
}

fn main() {
    let mut out_path = String::from("out.png");
    let mut force_fallback = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--fallback" => force_fallback = true,
            other => out_path = other.to_string(),
        }
    }
    pollster::block_on(run(&out_path, force_fallback));
}

async fn run(out_path: &str, force_fallback: bool) {
    // `.with_env()` lets WGPU_BACKEND / WGPU_ADAPTER_NAME steer selection — how a serve host
    // would pin itself to e.g. Vulkan+lavapipe on a GPU-less box.
    let instance = wgpu::Instance::new(
        wgpu::InstanceDescriptor::new_without_display_handle().with_env(),
    );

    let t0 = std::time::Instant::now();
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: force_fallback,
            compatible_surface: None, // <-- headless: no surface
            apply_limit_buckets: false,
        })
        .await
        .expect("no adapter — fallback ladder exhausted (rung 4: RenderError::NoAdapter)");

    let info = adapter.get_info();
    let is_software = info.device_type == wgpu::DeviceType::Cpu;
    println!("adapter:   {} ({:?})", info.name, info.backend);
    println!("driver:    {} {}", info.driver, info.driver_info);
    println!("device:    {:?}", info.device_type);
    println!("software:  {}", is_software);
    println!("fallback:  requested={}", force_fallback);

    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("spike-device"),
            required_limits: wgpu::Limits::downlevel_defaults(),
            ..Default::default()
        })
        .await
        .expect("request_device failed");

    // Offscreen render target.
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen"),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());

    // Geometry: one triangle, per-vertex colour.
    let verts = [
        Vertex { pos: [0.0, 0.7], color: [0.95, 0.35, 0.25] },
        Vertex { pos: [-0.7, -0.6], color: [0.30, 0.85, 0.45] },
        Vertex { pos: [0.7, -0.6], color: [0.30, 0.55, 0.95] },
    ];
    let vbuf = wgpu::util::DeviceExt::create_buffer_init(
        &device,
        &wgpu::util::BufferInitDescriptor {
            label: Some("verts"),
            contents: bytemuck::cast_slice(&verts),
            usage: wgpu::BufferUsages::VERTEX,
        },
    );

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("shader"),
        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(SHADER)),
    });

    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("layout"),
        bind_group_layouts: &[],
        immediate_size: 0,
    });

    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("pipeline"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Vertex>() as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x3],
            })],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: FORMAT,
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

    // Readback buffer, rows padded to COPY_BYTES_PER_ROW_ALIGNMENT (256).
    let unpadded_bpr = WIDTH * 4;
    let padded_bpr = align_up(unpadded_bpr, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded_bpr * HEIGHT) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("enc") });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.06,
                        g: 0.07,
                        b: 0.09,
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
        pass.set_pipeline(&pipeline);
        pass.set_vertex_buffer(0, vbuf.slice(..));
        pass.draw(0..3, 0..1);
    }

    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bpr),
                rows_per_image: Some(HEIGHT),
            },
        },
        wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
    );

    queue.submit([encoder.finish()]);

    // Map + wait (headless: synchronous drain, no event loop).
    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    rx.recv().expect("map channel").expect("map failed");

    // Un-pad rows into a tight RGBA8 image.
    let data = slice.get_mapped_range().expect("map range");
    let mut pixels = Vec::with_capacity((unpadded_bpr * HEIGHT) as usize);
    for row in 0..HEIGHT {
        let start = (row * padded_bpr) as usize;
        pixels.extend_from_slice(&data[start..start + unpadded_bpr as usize]);
    }
    drop(data);
    readback.unmap();

    image::save_buffer(out_path, &pixels, WIDTH, HEIGHT, image::ExtendedColorType::Rgba8)
        .expect("png save");

    println!("wrote:     {} ({}x{})", out_path, WIDTH, HEIGHT);
    println!("elapsed:   {:?} (adapter+device+render+readback)", t0.elapsed());
}
