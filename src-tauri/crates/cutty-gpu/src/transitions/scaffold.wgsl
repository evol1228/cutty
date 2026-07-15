// Transition pass: blend two premultiplied-alpha frame textures by a
// registry shader (`kind`) at `progress`. The per-transition functions
// below are ports from the MIT-licensed gl-transitions collection
// (https://github.com/gl-transitions/gl-transitions); each ported file
// carries its own attribution.
//
// Coordinate convention: `p` follows gl-transitions (origin bottom-left,
// +y up); the sampling helpers flip v once so ported code reads
// unchanged. Directional names ("wipe up") therefore match what happens
// on screen.
//
// The textures are premultiplied (layer coverage/opacity baked into
// alpha by the layer-premul pass), so mixing full RGBA vectors — what
// every port does — is the alpha-correct crossfade.

struct TransitionUniform {
    progress: f32,
    kind: u32,
    // Output aspect ratio (width / height), for ports that need it.
    ratio: f32,
    _pad0: f32,
    // 1 / output size in pixels (the inputs may be raw source textures
    // of any resolution — the direct fast path — so the pass derives
    // its coordinates from the output, never from an input).
    inv_size: vec2<f32>,
    _pad1: vec2<f32>,
}

@group(0) @binding(0) var<uniform> u: TransitionUniform;
@group(0) @binding(1) var from_tex: texture_2d<f32>;
@group(0) @binding(2) var to_tex: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    // Fullscreen triangle: (-1,-1) (3,-1) (-1,3).
    let x = f32(i32(vi & 1u) * 4 - 1);
    let y = f32(i32(vi >> 1u) * 4 - 1);
    return vec4<f32>(x, y, 0.0, 1.0);
}

fn getFromColor(p: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(from_tex, samp, vec2<f32>(p.x, 1.0 - p.y), 0.0);
}

fn getToColor(p: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(to_tex, samp, vec2<f32>(p.x, 1.0 - p.y), 0.0);
}

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let p = vec2<f32>(pos.x * u.inv_size.x, 1.0 - pos.y * u.inv_size.y);
    return transition_color(u.kind, p);
}
