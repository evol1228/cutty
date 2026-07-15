// Port of gl-transitions `windowslice`.
// https://github.com/gl-transitions/gl-transitions/blob/master/transitions/windowslice.glsl
// Author: gre — MIT License.
fn tr_windowslice(p: vec2<f32>) -> vec4<f32> {
    let count = 10.0;
    let smoothness = 0.5;
    let pr = smoothstep(-smoothness, 0.0, p.x - u.progress * (1.0 + smoothness));
    let s = step(pr, fract(count * p.x));
    return mix(getFromColor(p), getToColor(p), s);
}
