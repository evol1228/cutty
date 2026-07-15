//! The shared frame renderer: **one** implementation of "the output frame
//! at time `t`", consumed by both render frontends.
//!
//! - Preview (playback/scrub) renders proxies at preview resolution.
//! - Export renders originals at full output resolution.
//!
//! Everything that defines the picture lives here — the frame sampling
//! rule, layer geometry, opacity/blend parameters, and the GPU composite —
//! so preview == export holds *by construction*, enforced by the
//! golden-frame tests (`tests/golden_frames.rs`).
//!
//! # The frame sampling rule
//!
//! The output frame grid drives everything. For an output frame at time
//! `t`, [`cutty_engine::resolve_video_layers`] gives the active clips
//! bottom→top; for each layer, the frame shown is the **latest decoded
//! source frame with pts ≤ the clip's source time at `t`** (floor on the
//! source frame grid). A source that runs dry before its clip ends holds
//! its last frame (matching the fast export path's `tpad` semantics).
//!
//! Decoders advance forward frame-by-frame when the target is near
//! (sequential playback, small hops), and seek when it is not (scrubs,
//! jump cuts). One decoder serves each source across all tracks; the rare
//! project that shows the *same* source at two different offsets
//! simultaneously gets one decoder per occurrence.

use std::collections::HashMap;
use std::path::PathBuf;

use cutty_engine::{resolve_video_layers, Clip, Project, ProjectSettings};
use cutty_gpu::{BlendMode as GpuBlend, Compositor, Layer, SourceTexture, Target};

use crate::decode::SourceDecoder;
use crate::error::MediaError;

/// How many frames ahead a decoder will roll forward instead of seeking.
/// A seek costs up to one GOP of catch-up decode (proxies are `-g 30`),
/// so rolling forward is cheaper up to about that.
const FORWARD_WINDOW_FRAMES: i64 = 32;

/// Tolerance when mapping times to frame indices (matches `decode.rs`).
const PTS_EPS: f64 = 1e-6;

/// Renderer counters (logged by the frontends; asserted by perf tests).
#[derive(Debug, Default, Clone, Copy)]
pub struct RenderStats {
    /// Frames composited.
    pub frames: u64,
    /// Decoders opened synchronously inside a render (a potential hitch
    /// during playback; prefetch exists to keep this rare).
    pub cold_opens: u64,
    /// In-stream seeks (scrubs, jump cuts, backward samples).
    pub seeks: u64,
    /// Frames decoded rolling forward.
    pub forward_decodes: u64,
}

/// One open source: a decoder plus its GPU texture and what's in it.
struct SourceState {
    decoder: SourceDecoder,
    texture: SourceTexture,
    /// Source frame index currently uploaded to `texture`.
    uploaded_idx: Option<i64>,
}

impl SourceState {
    fn new(compositor: &Compositor, decoder: SourceDecoder) -> Self {
        let texture = compositor.create_source(decoder.width(), decoder.height());
        Self {
            decoder,
            texture,
            uploaded_idx: None,
        }
    }

    fn idx_of(&self, pts: f64) -> i64 {
        (pts * self.decoder.fps() + 0.5).floor() as i64
    }

    /// Make `texture` hold the latest source frame with pts ≤ `src_t`.
    /// Returns `false` when the stream has no frames at all (skip the
    /// layer). Holds the last frame past end of stream.
    fn sample(
        &mut self,
        compositor: &Compositor,
        src_t: f64,
        stats: &mut RenderStats,
    ) -> Result<bool, MediaError> {
        let fps = self.decoder.fps();
        let needed = (src_t.max(0.0) * fps + PTS_EPS).floor() as i64;

        if self.uploaded_idx == Some(needed) {
            return Ok(true);
        }

        // The decoder may already hold the wanted frame un-uploaded (a
        // freshly installed prefetched decoder sits exactly here).
        if let Some(pts) = self.decoder.current_pts_sec() {
            if self.idx_of(pts) == needed {
                return self.upload_current(compositor);
            }
        }

        // Past the end of a dried-up stream: hold the last frame.
        if self.decoder.is_exhausted() {
            if let Some(idx) = self.uploaded_idx {
                if needed > idx {
                    return Ok(true);
                }
            }
        }

        // Near-forward target: roll the stream forward (cheaper than a
        // seek). `next_pts_hint` is exact on CFR sources (all proxies);
        // VFR originals fall back to seeking.
        let forward_gap = self
            .decoder
            .next_pts_hint()
            .map(|next| needed - self.idx_of(next));
        if let Some(gap) = forward_gap {
            if (0..=FORWARD_WINDOW_FRAMES).contains(&gap) {
                loop {
                    let next_within = self
                        .decoder
                        .next_pts_hint()
                        .is_some_and(|next| self.idx_of(next) <= needed);
                    if !next_within {
                        break;
                    }
                    stats.forward_decodes += 1;
                    if self.decoder.next_frame()?.is_none() {
                        break; // dried up: hold what we have
                    }
                }
                return self.upload_current(compositor);
            }
        }

        // Everything else (backward, far forward, fresh or exhausted
        // stream): one seek positions the floor frame.
        stats.seeks += 1;
        let uploaded = {
            let Some(frame) = self.decoder.seek_to(src_t.max(0.0))? else {
                return Ok(self.uploaded_idx.is_some()); // no frames at all
            };
            let idx = (frame.pts_sec * fps + 0.5).floor() as i64;
            compositor.upload_rgba(&self.texture, frame.data, frame.stride);
            idx
        };
        self.uploaded_idx = Some(uploaded);
        Ok(true)
    }

    /// Upload the decoder's current frame (if any) to the texture.
    fn upload_current(&mut self, compositor: &Compositor) -> Result<bool, MediaError> {
        let fps = self.decoder.fps();
        let uploaded = {
            let Some(frame) = self.decoder.current_frame()? else {
                return Ok(self.uploaded_idx.is_some());
            };
            let idx = (frame.pts_sec * fps + 0.5).floor() as i64;
            compositor.upload_rgba(&self.texture, frame.data, frame.stride);
            idx
        };
        self.uploaded_idx = Some(uploaded);
        Ok(true)
    }
}

/// A layer resolved and sampled, ready to composite (phase 1 of
/// `begin_frame`; phase 2 turns these into borrowed [`Layer`]s).
struct PlannedLayer {
    key: (u64, u32),
    center: (f32, f32),
    size: (f32, f32),
    rotation_rad: f32,
    opacity: f32,
    blend: GpuBlend,
}

/// A composited output frame, borrowed from the readback buffer.
pub struct FrameSlice<'a> {
    pub width: u32,
    pub height: u32,
    /// Row pitch in bytes (≥ `width * 4`; GPU readbacks pad rows to 256).
    pub stride: usize,
    /// RGBA rows, `height * stride` bytes.
    pub data: &'a [u8],
}

/// Placement of a layer on the output, in output pixels. Pure math —
/// split out for unit tests.
///
/// The clip's source frame is fit inside the project canvas
/// (aspect-preserving contain, centered), then the clip transform is
/// applied in project space (x/y offset in project pixels, scale on the
/// fitted size), and the whole project canvas is uniformly fit into the
/// output. See [`cutty_engine::Transform`].
pub(crate) fn layer_placement(
    src_w: u32,
    src_h: u32,
    clip: &Clip,
    settings: &ProjectSettings,
    out_w: u32,
    out_h: u32,
) -> (f32, f32, f32, f32, f32) {
    let (pw, ph) = (settings.width as f64, settings.height as f64);
    let (ow, oh) = (f64::from(out_w), f64::from(out_h));
    // Project canvas → output mapping (letterboxed if aspects differ).
    let s = (ow / pw).min(oh / ph);
    let off_x = (ow - pw * s) / 2.0;
    let off_y = (oh - ph * s) / 2.0;
    // Source frame fit inside the project canvas.
    let base = (pw / f64::from(src_w)).min(ph / f64::from(src_h));
    let size_w = f64::from(src_w) * base * clip.transform.scale;
    let size_h = f64::from(src_h) * base * clip.transform.scale;
    let center_x = pw / 2.0 + clip.transform.x;
    let center_y = ph / 2.0 + clip.transform.y;
    (
        ((center_x * s) + off_x) as f32,
        ((center_y * s) + off_y) as f32,
        (size_w * s) as f32,
        (size_h * s) as f32,
        clip.transform.rotation.to_radians() as f32,
    )
}

fn gpu_blend(mode: cutty_engine::BlendMode) -> GpuBlend {
    match mode {
        cutty_engine::BlendMode::Normal => GpuBlend::Normal,
        cutty_engine::BlendMode::Multiply => GpuBlend::Multiply,
        cutty_engine::BlendMode::Screen => GpuBlend::Screen,
        cutty_engine::BlendMode::Overlay => GpuBlend::Overlay,
        cutty_engine::BlendMode::Add => GpuBlend::Add,
    }
}

/// Renders output frames for a project: decoders + GPU compositor behind
/// the frame sampling rule. Owned by one thread (playback control or the
/// export worker).
pub struct TimelineRenderer {
    compositor: Compositor,
    target: Target,
    out_w: u32,
    out_h: u32,
    /// `true`: any layer failure fails the frame (export). `false`: the
    /// layer is dropped and the message recorded (preview keeps playing).
    strict: bool,
    /// Keyed by (media id, per-frame occurrence index) — the occurrence
    /// index is nonzero only when one source is on screen twice at
    /// different offsets.
    sources: HashMap<(u64, u32), SourceState>,
    issues: Vec<String>,
    stats: RenderStats,
}

impl TimelineRenderer {
    /// Bring up the GPU and an output target of the given size.
    pub fn new(out_w: u32, out_h: u32, strict: bool) -> Result<Self, MediaError> {
        let compositor = Compositor::new().map_err(MediaError::Gpu)?;
        let target = compositor.create_target(out_w, out_h);
        Ok(Self {
            compositor,
            target,
            out_w,
            out_h,
            strict,
            sources: HashMap::new(),
            issues: Vec::new(),
            stats: RenderStats::default(),
        })
    }

    pub fn adapter_label(&self) -> String {
        self.compositor.adapter_label()
    }

    pub fn output_size(&self) -> (u32, u32) {
        (self.out_w, self.out_h)
    }

    pub fn stats(&self) -> RenderStats {
        self.stats
    }

    /// Non-fatal layer problems recorded since the last call (lenient
    /// mode only): missing sources, decode failures.
    pub fn take_issues(&mut self) -> Vec<String> {
        std::mem::take(&mut self.issues)
    }

    /// Whether a decoder for `media_id` is currently open.
    pub fn has_source(&self, media_id: u64) -> bool {
        self.sources.contains_key(&(media_id, 0))
    }

    /// Install a prefetched decoder for `media_id`, positioned near
    /// `needed_src_t`. Kept only when it beats the existing decoder (none,
    /// or one that would have to seek); otherwise dropped.
    pub fn offer_decoder(&mut self, media_id: u64, decoder: SourceDecoder, needed_src_t: f64) {
        let key = (media_id, 0);
        if let Some(existing) = self.sources.get(&key) {
            let fps = existing.decoder.fps();
            let needed = (needed_src_t.max(0.0) * fps + PTS_EPS).floor() as i64;
            let reachable = existing.uploaded_idx == Some(needed)
                || existing
                    .decoder
                    .current_pts_sec()
                    .is_some_and(|p| existing.idx_of(p) == needed)
                || existing.decoder.next_pts_hint().is_some_and(|next| {
                    let gap = needed - existing.idx_of(next);
                    (0..=FORWARD_WINDOW_FRAMES).contains(&gap)
                });
            if reachable {
                return; // the running decoder flows into this clip: keep it
            }
        }
        self.sources
            .insert(key, SourceState::new(&self.compositor, decoder));
    }

    /// Close decoders for sources not in `keep` (media ids).
    pub fn retain_sources(&mut self, keep: &std::collections::HashSet<u64>) {
        self.sources.retain(|(media, _), _| keep.contains(media));
    }

    /// Close every decoder (source files may have changed on disk).
    pub fn clear_sources(&mut self) {
        self.sources.clear();
    }

    /// Decode, upload, and composite the output frame at time `t` into
    /// readback slot `slot`, without blocking on the readback. Source
    /// files are resolved through `path_of` (proxy for preview, original
    /// for export). Pair with [`TimelineRenderer::read_frame`]; slots
    /// allow one frame's readback to overlap the next frame's work.
    pub fn begin_frame(
        &mut self,
        project: &Project,
        t: f64,
        path_of: &dyn Fn(u64) -> Option<PathBuf>,
        slot: usize,
    ) -> Result<(), MediaError> {
        let actives = resolve_video_layers(project, t);

        // Phase 1 (mutable): ensure a decoder per layer and sample it.
        let mut plan: Vec<PlannedLayer> = Vec::new();
        let mut occurrence: HashMap<u64, u32> = HashMap::new();
        for active in &actives {
            let Some((_, clip)) = project.find_clip(active.clip_id) else {
                continue;
            };
            let media = clip.media_id.0;
            let occ = occurrence.entry(media).or_insert(0);
            let key = (media, *occ);
            *occ += 1;

            if !self.sources.contains_key(&key) {
                let Some(path) = path_of(media) else {
                    self.layer_problem(format!(
                        "no renderable file for media {media} (proxy still generating?)"
                    ))?;
                    continue;
                };
                self.stats.cold_opens += 1;
                match SourceDecoder::open(&path) {
                    Ok(decoder) => {
                        self.sources
                            .insert(key, SourceState::new(&self.compositor, decoder));
                    }
                    Err(e) => {
                        self.layer_problem(format!("open {} failed: {e}", path.display()))?;
                        continue;
                    }
                }
            }

            let state = self.sources.get_mut(&key).expect("just ensured");
            match state.sample(&self.compositor, active.source_time, &mut self.stats) {
                Ok(true) => {
                    let (cx, cy, w, h, rot) = layer_placement(
                        state.texture.width(),
                        state.texture.height(),
                        clip,
                        &project.settings,
                        self.out_w,
                        self.out_h,
                    );
                    plan.push(PlannedLayer {
                        key,
                        center: (cx, cy),
                        size: (w, h),
                        rotation_rad: rot,
                        opacity: clip.opacity as f32,
                        blend: gpu_blend(clip.blend_mode),
                    });
                }
                Ok(false) => { /* stream with no frames: contributes nothing */ }
                Err(e) => {
                    // Broken decoder: drop it so the next frame reopens.
                    self.sources.remove(&key);
                    self.layer_problem(format!("decode failed: {e}"))?;
                }
            }
        }

        // Phase 2 (immutable): build layer refs and composite.
        let layers: Vec<Layer> = plan
            .iter()
            .map(|p| Layer {
                source: &self.sources[&p.key].texture,
                center: p.center,
                size: p.size,
                rotation_rad: p.rotation_rad,
                opacity: p.opacity,
                blend: p.blend,
            })
            .collect();
        self.compositor.composite(&mut self.target, &layers, slot);
        self.stats.frames += 1;
        Ok(())
    }

    /// Block until slot `slot`'s readback lands and hand the frame to `f`.
    pub fn read_frame<R>(
        &mut self,
        slot: usize,
        f: impl FnOnce(FrameSlice<'_>) -> R,
    ) -> Result<R, MediaError> {
        let (w, h) = (self.out_w, self.out_h);
        self.compositor
            .read_slot(&mut self.target, slot, |data, stride| {
                f(FrameSlice {
                    width: w,
                    height: h,
                    stride,
                    data,
                })
            })
            .map_err(MediaError::Gpu)
    }

    /// Render the frame at `t` and read it back synchronously (the
    /// preview path; export pipelines `begin_frame`/`read_frame` across
    /// two slots instead).
    pub fn render_with<R>(
        &mut self,
        project: &Project,
        t: f64,
        path_of: &dyn Fn(u64) -> Option<PathBuf>,
        f: impl FnOnce(FrameSlice<'_>) -> R,
    ) -> Result<R, MediaError> {
        self.begin_frame(project, t, path_of, 0)?;
        self.read_frame(0, f)
    }

    /// Record a layer problem: fail the frame in strict mode, log and
    /// drop the layer otherwise.
    fn layer_problem(&mut self, message: String) -> Result<(), MediaError> {
        if self.strict {
            return Err(MediaError::ExportNotReady { message });
        }
        self.issues.push(message);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutty_engine::{ClipId, MediaId, Transform};

    fn clip(transform: Transform) -> Clip {
        Clip {
            id: ClipId(1),
            media_id: MediaId(1),
            timeline_in: 0.0,
            timeline_out: 1.0,
            source_in: 0.0,
            source_out: 1.0,
            transform,
            opacity: 1.0,
            blend_mode: cutty_engine::BlendMode::Normal,
            speed: 1.0,
            volume: 1.0,
        }
    }

    fn settings(w: u32, h: u32) -> ProjectSettings {
        ProjectSettings {
            width: w,
            height: h,
            fps: 30.0,
        }
    }

    #[test]
    fn default_transform_fits_and_centers() {
        // 16:9 source in a 16:9 project rendered at 1280×720: exact cover.
        let (cx, cy, w, h, rot) = layer_placement(
            1920,
            1080,
            &clip(Transform::default()),
            &settings(1920, 1080),
            1280,
            720,
        );
        assert_eq!((cx, cy), (640.0, 360.0));
        assert_eq!((w, h), (1280.0, 720.0));
        assert_eq!(rot, 0.0);

        // Vertical source letterboxed into a 16:9 project: fit by height.
        let (cx, cy, w, h, _) = layer_placement(
            720,
            1280,
            &clip(Transform::default()),
            &settings(1920, 1080),
            1920,
            1080,
        );
        assert_eq!((cx, cy), (960.0, 540.0));
        assert_eq!(h, 1080.0);
        assert!((w - 607.5).abs() < 0.01, "w = {w}");
    }

    #[test]
    fn transform_offsets_scale_with_output_resolution() {
        // +100 px x-offset and 0.5 scale in a 1080p project…
        let t = Transform {
            x: 100.0,
            y: -50.0,
            scale: 0.5,
            rotation: 90.0,
        };
        // …rendered at project size: offset applies 1:1.
        let (cx, cy, w, h, rot) = layer_placement(
            1920,
            1080,
            &clip(t.clone()),
            &settings(1920, 1080),
            1920,
            1080,
        );
        assert_eq!((cx, cy), (1060.0, 490.0));
        assert_eq!((w, h), (960.0, 540.0));
        assert!((rot - std::f32::consts::FRAC_PI_2).abs() < 1e-6);

        // …rendered at half size (preview): everything halves.
        let (cx, cy, w, h, _) =
            layer_placement(1920, 1080, &clip(t), &settings(1920, 1080), 960, 540);
        assert_eq!((cx, cy), (530.0, 245.0));
        assert_eq!((w, h), (480.0, 270.0));
    }

    #[test]
    fn output_aspect_mismatch_letterboxes_the_project_canvas() {
        // 16:9 project exported to a square output: the canvas maps to
        // the middle band, so the fitted clip does too.
        let (cx, cy, w, h, _) = layer_placement(
            1920,
            1080,
            &clip(Transform::default()),
            &settings(1920, 1080),
            1000,
            1000,
        );
        assert_eq!((cx, cy), (500.0, 500.0));
        assert!((w - 1000.0).abs() < 0.01);
        assert!((h - 562.5).abs() < 0.01);
    }
}
