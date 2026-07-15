// Port of gl-transitions `cube`.
// https://github.com/gl-transitions/gl-transitions/blob/master/transitions/cube.glsl
// Author: gre — MIT License. Adapted: the background/reflection floor is
// transparent instead of opaque black, so the effect composites
// correctly as a layer.
fn cube_project(p: vec2<f32>) -> vec2<f32> {
    // floating = 3.0
    return p * vec2<f32>(1.0, -1.2) + vec2<f32>(0.0, -0.03);
}
fn cube_in_bounds(p: vec2<f32>) -> bool {
    return all(p > vec2<f32>(0.0)) && all(p < vec2<f32>(1.0));
}
fn cube_bg(pfr_in: vec2<f32>, pto_in: vec2<f32>) -> vec4<f32> {
    let reflection = 0.4;
    var c = vec4<f32>(0.0);
    let pfr = cube_project(pfr_in);
    if (cube_in_bounds(pfr)) {
        c += mix(vec4<f32>(0.0), getFromColor(pfr), reflection * mix(1.0, 0.0, pfr.y));
    }
    let pto = cube_project(pto_in);
    if (cube_in_bounds(pto)) {
        c += mix(vec4<f32>(0.0), getToColor(pto), reflection * mix(1.0, 0.0, pto.y));
    }
    return c;
}
fn cube_xskew(p: vec2<f32>, persp: f32, center: f32) -> vec2<f32> {
    let x = mix(p.x, 1.0 - p.x, center);
    let base = vec2<f32>(x, (p.y - 0.5 * (1.0 - persp) * x) / (1.0 + (persp - 1.0) * x))
        - vec2<f32>(0.5 - abs(center - 0.5), 0.0);
    let flip = select(-1.0, 1.0, center < 0.5);
    return base * vec2<f32>(0.5 / abs(center - 0.5) * flip, 1.0)
        + vec2<f32>(select(1.0, 0.0, center < 0.5), 0.0);
}
fn tr_cube(op: vec2<f32>) -> vec4<f32> {
    let persp = 0.7;
    let unzoom = 0.3;
    let uz = unzoom * 2.0 * (0.5 - abs(0.5 - u.progress));
    let p = -uz * 0.5 + (1.0 + uz) * op;
    let from_p = cube_xskew(
        (p - vec2<f32>(u.progress, 0.0)) / vec2<f32>(1.0 - u.progress, 1.0),
        1.0 - mix(u.progress, 0.0, persp),
        0.0,
    );
    let to_p = cube_xskew(
        p / vec2<f32>(u.progress, 1.0),
        mix(u.progress * u.progress, 1.0, persp),
        1.0,
    );
    if (cube_in_bounds(from_p)) {
        return getFromColor(from_p);
    }
    if (cube_in_bounds(to_p)) {
        return getToColor(to_p);
    }
    return cube_bg(from_p, to_p);
}
