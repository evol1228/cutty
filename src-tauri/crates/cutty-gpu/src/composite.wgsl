// Layer compositing: one full-target pass per layer.
//
// Each pass reads the accumulated result so far (`accum_tex`), samples the
// layer's source texture through an inverse placement transform, blends
// where the layer covers the output, and passes the accumulator through
// everywhere else. Ping-ponging between two target textures gives every
// blend mode access to the backdrop (fixed-function blending can't
// express multiply/screen/overlay).
//
// Compositing math runs directly on sRGB-encoded values: the textures are
// `Rgba8Unorm` (not `-Srgb`), so no implicit linearization happens on
// sample or store. This matches CSS/most editors. Linear-light
// compositing is a known future correctness upgrade — when it lands it
// must land in this one shader so preview and export change together.

struct LayerUniform {
    // Inverse placement: output pixel center (x, y, 1) → source UV.
    inv_row0: vec4<f32>,
    inv_row1: vec4<f32>,
    // x: opacity 0..1 · y: blend mode as u32 bits · z, w: unused.
    opacity: f32,
    blend: u32,
    _pad0: f32,
    _pad1: f32,
}

@group(0) @binding(0) var<uniform> layer: LayerUniform;
@group(0) @binding(1) var accum_tex: texture_2d<f32>;
@group(0) @binding(2) var layer_tex: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    // Fullscreen triangle: (-1,-1) (3,-1) (-1,3).
    let x = f32(i32(vi & 1u) * 4 - 1);
    let y = f32(i32(vi >> 1u) * 4 - 1);
    return vec4<f32>(x, y, 0.0, 1.0);
}

// W3C separable blend modes, per channel, backdrop `b` under source `s`.
fn overlay_channel(b: f32, s: f32) -> f32 {
    return select(1.0 - 2.0 * (1.0 - s) * (1.0 - b), 2.0 * s * b, b <= 0.5);
}

// Inverse placement: output pixel center → source UV.
fn layer_uv(pos: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(
        layer.inv_row0.x * pos.x + layer.inv_row0.y * pos.y + layer.inv_row0.z,
        layer.inv_row1.x * pos.x + layer.inv_row1.y * pos.y + layer.inv_row1.z,
    );
}

// Coverage: 1 inside the layer quad, 0 outside, with a one-texel linear
// ramp at the border (cheap deterministic edge AA — matters for rotated
// layers).
fn layer_coverage(uv: vec2<f32>) -> f32 {
    let src_dims = vec2<f32>(textureDimensions(layer_tex));
    let dist_px = min(uv, vec2<f32>(1.0) - uv) * src_dims;
    return clamp(min(dist_px.x, dist_px.y) + 0.5, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let out_dims = vec2<f32>(textureDimensions(accum_tex));
    let dst = textureSampleLevel(accum_tex, samp, pos.xy / out_dims, 0.0);

    let uv = layer_uv(pos.xy);
    let src = textureSampleLevel(layer_tex, samp, uv, 0.0);
    let coverage = layer_coverage(uv);

    // Blend 5: the source is premultiplied (a transition pass output) —
    // straight "premultiplied over" against the opaque backdrop.
    if (layer.blend == 5u) {
        let g = layer.opacity * coverage;
        return vec4<f32>(src.rgb * g + dst.rgb * (1.0 - src.a * g), 1.0);
    }

    var blended: vec3<f32>;
    switch layer.blend {
        case 0u: { blended = src.rgb; }                                  // normal
        case 1u: { blended = src.rgb * dst.rgb; }                        // multiply
        case 2u: { blended = src.rgb + dst.rgb - src.rgb * dst.rgb; }    // screen
        case 3u: {                                                       // overlay
            blended = vec3<f32>(
                overlay_channel(dst.r, src.r),
                overlay_channel(dst.g, src.g),
                overlay_channel(dst.b, src.b),
            );
        }
        default: { blended = min(src.rgb + dst.rgb, vec3<f32>(1.0)); }   // add
    }

    let a = layer.opacity * src.a * coverage;
    return vec4<f32>(mix(dst.rgb, blended, a), 1.0);
}

// Transition intermediates: one layer rendered over *transparency* with
// premultiplied alpha (opacity and edge coverage baked into α). The
// transition pass blends two of these; the result re-enters the main
// stack through the blend-5 path above. Uses bindings 0/2/3 only — the
// accumulator is not read.
@fragment
fn fs_layer_premul(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let uv = layer_uv(pos.xy);
    let src = textureSampleLevel(layer_tex, samp, uv, 0.0);
    let a = layer.opacity * src.a * layer_coverage(uv);
    return vec4<f32>(src.rgb * a, a);
}
