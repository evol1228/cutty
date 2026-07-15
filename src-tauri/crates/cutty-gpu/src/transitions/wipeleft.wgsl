// Port of gl-transitions `wipeLeft`.
// https://github.com/gl-transitions/gl-transitions/blob/master/transitions/wipeLeft.glsl
// Author: Jake Nelson — MIT License.
fn tr_wipeleft(p: vec2<f32>) -> vec4<f32> {
    return mix(getFromColor(p), getToColor(p), step(1.0 - p.x, u.progress));
}
