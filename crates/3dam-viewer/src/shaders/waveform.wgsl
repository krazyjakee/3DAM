// Audio waveform island shader. Draws pre-reduced min/max bars (built on the CPU per canvas
// column) and colours each fragment by whether its track position is before or after the playhead.

struct WaveUniforms {
    played   : vec4<f32>,
    unplayed : vec4<f32>,
    progress : f32,
    _pad0 : f32, _pad1 : f32, _pad2 : f32,
};

@group(0) @binding(0) var<uniform> u : WaveUniforms;

struct VsOut {
    @builtin(position) clip : vec4<f32>,
    @location(0)       x01  : f32,   // 0..1 position along the track
};

@vertex
fn vs_main(@location(0) pos : vec2<f32>, @location(1) x01 : f32) -> VsOut {
    var out : VsOut;
    out.clip = vec4<f32>(pos, 0.0, 1.0);
    out.x01 = x01;
    return out;
}

@fragment
fn fs_main(in : VsOut) -> @location(0) vec4<f32> {
    if (in.x01 <= u.progress) {
        return u.played;
    }
    return u.unplayed;
}
