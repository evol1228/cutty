//! # cutty-gpu
//!
//! GPU compositor: wgpu pipelines and WGSL shaders for compositing,
//! transforms, effects, and transitions. All pixel work outside of
//! decode/encode runs here.
//!
//! Phase 0: intentionally empty skeleton. The wgpu compositor lands in
//! Phase 2 — Phase 0 playback streams decoded frames directly.

#[cfg(test)]
mod tests {
    #[test]
    fn skeleton_compiles() {}
}
