// Port of gl-transitions `fade` (crossfade/dissolve).
// https://github.com/gl-transitions/gl-transitions/blob/master/transitions/fade.glsl
// Author: gre — MIT License.
fn tr_fade(p: vec2<f32>) -> vec4<f32> {
    return mix(getFromColor(p), getToColor(p), u.progress);
}
