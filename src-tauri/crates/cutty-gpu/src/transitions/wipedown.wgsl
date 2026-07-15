// Port of gl-transitions `wipeDown`.
// https://github.com/gl-transitions/gl-transitions/blob/master/transitions/wipeDown.glsl
// Author: Jake Nelson — MIT License.
fn tr_wipedown(p: vec2<f32>) -> vec4<f32> {
    return mix(getFromColor(p), getToColor(p), step(1.0 - p.y, u.progress));
}
