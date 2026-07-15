// Port of gl-transitions `Radial`.
// https://github.com/gl-transitions/gl-transitions/blob/master/transitions/Radial.glsl
// Author: Xaychru — MIT License.
fn tr_radial(p_in: vec2<f32>) -> vec4<f32> {
    let smoothness = 1.0;
    let pi = 3.141592653589;
    let rp = p_in * 2.0 - 1.0;
    return mix(
        getToColor(p_in),
        getFromColor(p_in),
        smoothstep(0.0, smoothness, atan2(rp.y, rp.x) - (u.progress - 0.5) * pi * 2.5),
    );
}
