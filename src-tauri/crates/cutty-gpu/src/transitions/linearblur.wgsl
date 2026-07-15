// Port of gl-transitions `LinearBlur` (the blur dissolve).
// https://github.com/gl-transitions/gl-transitions/blob/master/transitions/LinearBlur.glsl
// Author: gre — MIT License. Adapted for realtime 720p on integrated
// GPUs (upstream's 6×6 grid measured 37 ms/frame on Intel UHD 630 vs a
// 33 ms budget): 3×3 taps with a per-pixel jitter of the grid (upstream
// has none — noise reads better than banding at the lower count) and a
// slightly tighter blur radius to match the coarser sampling.
fn lb_rand(co: vec2<f32>) -> f32 {
    return fract(sin(dot(co, vec2<f32>(12.9898, 78.233))) * 43758.5453);
}
fn tr_linearblur(p: vec2<f32>) -> vec4<f32> {
    let intensity = 0.07;
    let passes = 3.0;
    var c1 = vec4<f32>(0.0);
    var c2 = vec4<f32>(0.0);
    let disp = intensity * (0.5 - abs(0.5 - u.progress));
    let jitter = (lb_rand(p) - 0.5) / passes;
    for (var xi = 0; xi < 3; xi += 1) {
        let x = f32(xi) / passes - 0.5 + jitter;
        for (var yi = 0; yi < 3; yi += 1) {
            let y = f32(yi) / passes - 0.5 + jitter;
            let v = vec2<f32>(x, y);
            c1 += getFromColor(p + disp * v);
            c2 += getToColor(p + disp * v);
        }
    }
    return mix(c1 / 9.0, c2 / 9.0, u.progress);
}
