// Port of gl-transitions `GlitchMemories`.
// https://github.com/gl-transitions/gl-transitions/blob/master/transitions/GlitchMemories.glsl
// Author: Gunnar Roth (based on work by natewave) — MIT License.
// Adapted: alpha follows the base crossfade instead of a constant 1, so
// the effect composites correctly as a layer.
fn tr_glitchmemories(p: vec2<f32>) -> vec4<f32> {
    let block = floor(p / vec2<f32>(16.0));
    var uv_noise = block / vec2<f32>(64.0);
    uv_noise += floor(vec2<f32>(u.progress) * vec2<f32>(1200.0, 3500.0)) / vec2<f32>(64.0);
    var dist = vec2<f32>(0.0);
    if (u.progress > 0.0) {
        dist = (fract(uv_noise) - 0.5) * 0.3 * (1.0 - u.progress);
    }
    let red = p + dist * 0.2;
    let green = p + dist * 0.3;
    let blue = p + dist * 0.5;
    let base = mix(getFromColor(p), getToColor(p), u.progress);
    return vec4<f32>(
        mix(getFromColor(red), getToColor(red), u.progress).r,
        mix(getFromColor(green), getToColor(green), u.progress).g,
        mix(getFromColor(blue), getToColor(blue), u.progress).b,
        base.a,
    );
}
