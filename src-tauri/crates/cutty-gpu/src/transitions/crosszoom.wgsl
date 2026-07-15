// Port of gl-transitions `CrossZoom`.
// https://github.com/gl-transitions/gl-transitions/blob/master/transitions/CrossZoom.glsl
// Author: rectalogic — MIT License (ported from Boundless, based on a
// Pete Warden shader). Adapted: full RGBA accumulation instead of RGB
// with constant alpha, so the effect composites correctly as a layer;
// and the streak loop is 10 taps instead of upstream's 41 — the
// per-pixel `rand` offset already dithers the samples, and 41 taps
// cannot hold 30 fps at 720p on integrated GPUs (measured 42 ms/frame
// on Intel UHD 630 vs a 33 ms budget).
fn cz_expo_ease_in_out(t: f32) -> f32 {
    if (t <= 0.0) {
        return 0.0;
    }
    if (t >= 1.0) {
        return 1.0;
    }
    let s = t * 2.0;
    if (s < 1.0) {
        return 0.5 * pow(2.0, 10.0 * (s - 1.0));
    }
    return 0.5 * (2.0 - pow(2.0, -10.0 * (s - 1.0)));
}
fn cz_sin_ease_in_out(t: f32, change: f32, duration: f32) -> f32 {
    let pi = 3.141592653589793;
    return -change / 2.0 * (cos(pi * t / duration) - 1.0);
}
fn cz_rand(co: vec2<f32>) -> f32 {
    return fract(sin(dot(co, vec2<f32>(12.9898, 78.233))) * 43758.5453);
}
fn tr_crosszoom(p: vec2<f32>) -> vec4<f32> {
    let strength = 0.4;
    let dissolve = cz_expo_ease_in_out(u.progress);
    let ease_t = select(1.0 - u.progress, u.progress, u.progress < 0.5);
    let strength_amt = cz_sin_ease_in_out(ease_t, strength, 0.5);
    var color = vec4<f32>(0.0);
    var total = 0.0;
    let to_center = vec2<f32>(0.5) - p;
    let offset = cz_rand(p);
    for (var i = 0.0; i <= 9.0; i += 1.0) {
        let percent = (i + offset) / 9.0;
        let weight = 4.0 * (percent - percent * percent);
        let q = p + to_center * percent * strength_amt;
        color += mix(getFromColor(q), getToColor(q), dissolve) * weight;
        total += weight;
    }
    return color / total;
}
