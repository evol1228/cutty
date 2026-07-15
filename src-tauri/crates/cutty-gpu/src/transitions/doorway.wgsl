// Port of gl-transitions `doorway`.
// https://github.com/gl-transitions/gl-transitions/blob/master/transitions/doorway.glsl
// Author: gre — MIT License. Adapted: transparent background instead of
// opaque black (layer compositing), and the door test is `>= 0` so
// progress 0 shows the untouched FROM frame on the center column too.
fn dw_project(p: vec2<f32>) -> vec2<f32> {
    return p * vec2<f32>(1.0, -1.2) + vec2<f32>(0.0, -0.02);
}
fn dw_in_bounds(p: vec2<f32>) -> bool {
    return all(p > vec2<f32>(0.0)) && all(p < vec2<f32>(1.0));
}
fn dw_bg(pto_in: vec2<f32>) -> vec4<f32> {
    let reflection = 0.4;
    var c = vec4<f32>(0.0);
    let pto = dw_project(pto_in);
    if (dw_in_bounds(pto)) {
        c += mix(vec4<f32>(0.0), getToColor(pto), reflection * mix(1.0, 0.0, pto.y));
    }
    return c;
}
fn tr_doorway(p: vec2<f32>) -> vec4<f32> {
    let perspective = 0.4;
    let depth = 3.0;
    var pfr = vec2<f32>(-1.0);
    let middle_slit = 2.0 * abs(p.x - 0.5) - u.progress;
    if (middle_slit >= 0.0) {
        pfr = p + select(1.0, -1.0, p.x > 0.5) * vec2<f32>(0.5 * u.progress, 0.0);
        let d = 1.0 / (1.0 + perspective * u.progress * (1.0 - middle_slit));
        pfr = vec2<f32>((pfr.x - 0.5) * d + 0.5, (pfr.y - 0.5) * d + 0.5);
    }
    let size = mix(1.0, depth, 1.0 - u.progress);
    let pto = (p + vec2<f32>(-0.5, -0.5)) * vec2<f32>(size, size) + vec2<f32>(0.5, 0.5);
    if (dw_in_bounds(pfr)) {
        return getFromColor(pfr);
    }
    if (dw_in_bounds(pto)) {
        return getToColor(pto);
    }
    return dw_bg(pto);
}
