// Port of gl-transitions `pixelize`.
// https://github.com/gl-transitions/gl-transitions/blob/master/transitions/pixelize.glsl
// Author: gre — MIT License.
fn tr_pixelize(p_in: vec2<f32>) -> vec4<f32> {
    let squares_min = vec2<f32>(20.0, 20.0);
    let steps = 50.0;
    let d = min(u.progress, 1.0 - u.progress);
    let dist = ceil(d * steps) / steps;
    var p = p_in;
    if (dist > 0.0) {
        let square_size = 2.0 * dist / squares_min;
        p = (floor(p_in / square_size) + 0.5) * square_size;
    }
    return mix(getFromColor(p), getToColor(p), u.progress);
}
