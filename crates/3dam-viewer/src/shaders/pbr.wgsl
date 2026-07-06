// PBR-lite — the single shader set the 3D island uses (tech-spec 06 §9).
//
// Base colour + a fixed key/fill directional light rig + a little ambient/hemisphere fill. Simple
// and stable on purpose: the goal is *consistent* framing/shading with the server thumbnail, not
// photoreal. Metallic/roughness is a constant here (viewer-grade); when this reconciles with
// `dam-render` it grows per-material inputs.

struct Uniforms {
    view_proj : mat4x4<f32>,
    eye       : vec4<f32>,   // world-space camera position; .w unused
};

@group(0) @binding(0) var<uniform> u : Uniforms;

struct VsOut {
    @builtin(position) clip   : vec4<f32>,
    @location(0)       world  : vec3<f32>,
    @location(1)       normal : vec3<f32>,
    @location(2)       color  : vec3<f32>,
};

@vertex
fn vs_main(
    @location(0) pos    : vec3<f32>,
    @location(1) normal : vec3<f32>,
    @location(2) color  : vec3<f32>,
) -> VsOut {
    var out : VsOut;
    out.clip = u.view_proj * vec4<f32>(pos, 1.0);
    out.world = pos;
    out.normal = normal;
    out.color = color;
    return out;
}

// Fixed light rig (view-independent world directions), matching the "gentle key + soft fill"
// intent of the thumbnail rig.
const KEY_DIR  : vec3<f32> = vec3<f32>(0.4, 0.8, 0.55);
const FILL_DIR : vec3<f32> = vec3<f32>(-0.5, 0.3, -0.4);
const KEY_COL  : vec3<f32> = vec3<f32>(1.0, 0.98, 0.94);
const FILL_COL : vec3<f32> = vec3<f32>(0.32, 0.36, 0.45);
// Hemisphere ambient: cool up, warm-ish down — keeps unlit faces from going pure black.
const SKY_COL    : vec3<f32> = vec3<f32>(0.20, 0.23, 0.28);
const GROUND_COL : vec3<f32> = vec3<f32>(0.10, 0.09, 0.08);

fn shade(n : vec3<f32>, v : vec3<f32>, base : vec3<f32>) -> vec3<f32> {
    let key_l  = normalize(KEY_DIR);
    let fill_l = normalize(FILL_DIR);

    // Lambert diffuse from both lights.
    let diff = base * (KEY_COL * max(dot(n, key_l), 0.0)
                     + FILL_COL * max(dot(n, fill_l), 0.0));

    // Blinn-Phong-ish specular from the key light only.
    let h = normalize(key_l + v);
    let spec = KEY_COL * pow(max(dot(n, h), 0.0), 32.0) * 0.25;

    // Hemisphere ambient.
    let hemi = mix(GROUND_COL, SKY_COL, n.y * 0.5 + 0.5) * base;

    return diff + spec + hemi;
}

@fragment
fn fs_main(in : VsOut) -> @location(0) vec4<f32> {
    // Renormalise (interpolation shrinks normals); flip toward the viewer so back-lit thin geo and
    // any residual winding issues still read.
    var n = normalize(in.normal);
    let v = normalize(u.eye.xyz - in.world);
    if (dot(n, v) < 0.0) {
        n = -n;
    }
    var c = shade(n, v, in.color);
    // Cheap gamma so the linear lighting doesn't look flat on an sRGB canvas.
    c = pow(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(1.0 / 2.2));
    return vec4<f32>(c, 1.0);
}
