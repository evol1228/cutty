// Port of gl-transitions `directional` (the sliding push), with the
// direction baked per registry entry.
// https://github.com/gl-transitions/gl-transitions/blob/master/transitions/directional.glsl
// Author: Gaëtan Renaudeau — MIT License.
fn slide_dir(uv: vec2<f32>, direction: vec2<f32>) -> vec4<f32> {
    let p = uv + u.progress * sign(direction);
    let f = fract(p);
    let inside = step(0.0, p.y) * step(p.y, 1.0) * step(0.0, p.x) * step(p.x, 1.0);
    return mix(getToColor(f), getFromColor(f), inside);
}
fn tr_slideleft(p: vec2<f32>) -> vec4<f32> {
    return slide_dir(p, vec2<f32>(1.0, 0.0));
}
fn tr_slideright(p: vec2<f32>) -> vec4<f32> {
    return slide_dir(p, vec2<f32>(-1.0, 0.0));
}
