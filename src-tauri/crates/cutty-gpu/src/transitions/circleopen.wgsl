// Port of gl-transitions `circleopen` (both directions of its `opening`
// uniform, exposed as two registry entries).
// https://github.com/gl-transitions/gl-transitions/blob/master/transitions/circleopen.glsl
// Author: gre — MIT License.
fn circle_wipe(p: vec2<f32>, opening: bool) -> vec4<f32> {
    let smoothness = 0.3;
    let center = vec2<f32>(0.5, 0.5);
    let sqrt_2 = 1.414213562373;
    let x = select(1.0 - u.progress, u.progress, opening);
    let m = smoothstep(-smoothness, 0.0, sqrt_2 * distance(center, p) - x * (1.0 + smoothness));
    return mix(getFromColor(p), getToColor(p), select(m, 1.0 - m, opening));
}
fn tr_circleopen(p: vec2<f32>) -> vec4<f32> {
    return circle_wipe(p, true);
}
fn tr_circleclose(p: vec2<f32>) -> vec4<f32> {
    return circle_wipe(p, false);
}
