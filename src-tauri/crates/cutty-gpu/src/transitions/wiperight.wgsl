// Port of gl-transitions `wipeRight`.
// https://github.com/gl-transitions/gl-transitions/blob/master/transitions/wipeRight.glsl
// Author: Jake Nelson — MIT License.
fn tr_wiperight(p: vec2<f32>) -> vec4<f32> {
    return mix(getFromColor(p), getToColor(p), step(p.x, u.progress));
}
