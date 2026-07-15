// Port of gl-transitions `wipeUp`.
// https://github.com/gl-transitions/gl-transitions/blob/master/transitions/wipeUp.glsl
// Author: Jake Nelson — MIT License.
fn tr_wipeup(p: vec2<f32>) -> vec4<f32> {
    return mix(getFromColor(p), getToColor(p), step(p.y, u.progress));
}
