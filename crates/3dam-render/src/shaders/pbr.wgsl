// Metallic-roughness PBR for turntable thumbnails (tech-spec 06). Positions/normals/tangents arrive
// already in world space (node transforms baked by Assimp PreTransformVertices), so the only matrix
// is the view-projection. A fixed 3-light studio rig + hemispheric ambient gives a neutral,
// consistent, professional read of surface + material across all asset types.

struct Globals {
    view_proj: mat4x4<f32>,
    camera_pos: vec4<f32>,
};

struct MaterialU {
    base_color: vec4<f32>,  // rgba factor (a = opacity)
    mr: vec4<f32>,          // x = metallic, y = roughness, z = alpha cutoff, w = alpha mode (0/1/2)
    emissive: vec4<f32>,    // rgb factor
    flags: vec4<f32>,       // x has_base, y has_mr, z has_normal, w has_emissive
};

@group(0) @binding(0) var<uniform> globals: Globals;

@group(1) @binding(0) var<uniform> mat: MaterialU;
@group(1) @binding(1) var samp: sampler;
@group(1) @binding(2) var base_tex: texture_2d<f32>;
@group(1) @binding(3) var mr_tex: texture_2d<f32>;
@group(1) @binding(4) var normal_tex: texture_2d<f32>;
@group(1) @binding(5) var emissive_tex: texture_2d<f32>;

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) world_pos: vec3<f32>,
    @location(1) world_normal: vec3<f32>,
    @location(2) world_tangent: vec3<f32>,
    @location(3) tangent_w: f32,
    @location(4) uv: vec2<f32>,
    @location(5) color: vec4<f32>,
};

@vertex
fn vs_main(
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) tangent: vec4<f32>,
    @location(3) uv: vec2<f32>,
    @location(4) color: vec4<f32>,
) -> VsOut {
    var out: VsOut;
    out.clip_pos = globals.view_proj * vec4<f32>(pos, 1.0);
    out.world_pos = pos;
    out.world_normal = normal;
    out.world_tangent = tangent.xyz;
    out.tangent_w = tangent.w;
    out.uv = uv;
    out.color = color;
    return out;
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
    let g = geometry_smith(n_dot_v, n_dot_l, rough);
    let f = fresnel_schlick(max(dot(h, v), 0.0), f0);

    let spec = (ndf * g * f) / max(4.0 * n_dot_v * n_dot_l, 1e-4);
    let kd = (vec3<f32>(1.0) - f) * (1.0 - metallic);
    let diffuse = kd * albedo / PI;
    return (diffuse + spec) * l_color * n_dot_l;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Albedo (base colour texture is sRGB → linear on sample) × factor × vertex colour. The base
    // texture's alpha (linear, not sRGB-encoded) multiplies the base-colour opacity for mask/blend.
    var albedo = mat.base_color.rgb * in.color.rgb;
    var alpha = mat.base_color.a * in.color.a;
    if (mat.flags.x > 0.5) {
        let bs = textureSample(base_tex, samp, in.uv);
        albedo = albedo * bs.rgb;
        alpha = alpha * bs.a;
    }

    var metallic = mat.mr.x;
    var rough = mat.mr.y;
    if (mat.flags.y > 0.5) {
        let mr = textureSample(mr_tex, samp, in.uv);
        rough = rough * mr.g;   // glTF packs roughness in G, metallic in B
        metallic = metallic * mr.b;
    }
    rough = clamp(rough, 0.04, 1.0);
    metallic = clamp(metallic, 0.0, 1.0);

    // Geometric normal, with optional tangent-space normal map.
    var n = normalize(in.world_normal);
    if (mat.flags.z > 0.5) {
        let t = normalize(in.world_tangent - n * dot(n, in.world_tangent));
        let b = cross(n, t) * in.tangent_w;
        let sampled = textureSample(normal_tex, samp, in.uv).xyz * 2.0 - 1.0;
        let tbn = mat3x3<f32>(t, b, n);
        n = normalize(tbn * sampled);
    }

    let v = normalize(globals.camera_pos.xyz - in.world_pos);
    let f0 = mix(vec3<f32>(0.04), albedo, metallic);

    // Fixed studio rig (world space): warm key, cool fill, back rim.
    var lo = vec3<f32>(0.0);
    lo = lo + direct_light(vec3<f32>(0.5, 0.8, 0.6), vec3<f32>(3.0, 2.9, 2.7), n, v, albedo, metallic, rough, f0);
    lo = lo + direct_light(vec3<f32>(-0.6, 0.3, 0.4), vec3<f32>(0.7, 0.8, 1.0), n, v, albedo, metallic, rough, f0);
    lo = lo + direct_light(vec3<f32>(-0.2, 0.5, -0.9), vec3<f32>(1.1, 1.1, 1.4), n, v, albedo, metallic, rough, f0);

    // Hemispheric ambient (cheap IBL substitute): sky above, darker ground below.
    let sky = vec3<f32>(0.40, 0.44, 0.52);
    let ground = vec3<f32>(0.11, 0.10, 0.10);
    let hemi = mix(ground, sky, clamp(n.y * 0.5 + 0.5, 0.0, 1.0));
    let ambient = hemi * albedo * (1.0 - metallic * 0.6);
    lo = lo + ambient;

    // Emissive.
    if (mat.flags.w > 0.5) {
        lo = lo + mat.emissive.rgb * textureSample(emissive_tex, samp, in.uv).rgb;
    } else {
        lo = lo + mat.emissive.rgb;
    }

    // Reinhard tone-map + write linear to the sRGB target (hardware encodes).
    let mapped = lo / (lo + vec3<f32>(1.0));

    // Alpha mode: 0 opaque, 1 mask, 2 blend. Opaque/mask write fully opaque (mask discards below its
    // cutoff — deferred to here so texture sampling stays in uniform control flow). Blend keeps the
    // opacity, firmed up at grazing angles by a Fresnel term so glass reads as glass: see-through
    // face-on, reflective at the silhouette.
    let alpha_mode = mat.mr.w;
    let cutoff = mat.mr.z;
    var out_a = 1.0;
    if (alpha_mode > 1.5) {
        let ndv = max(dot(n, v), 0.0);
        let fres = pow(1.0 - ndv, 5.0);
        out_a = clamp(alpha + (1.0 - alpha) * fres, 0.0, 1.0);
    } else if (alpha_mode > 0.5) {
        if (alpha < cutoff) { discard; }
    }
    return vec4<f32>(mapped, out_a);
}
